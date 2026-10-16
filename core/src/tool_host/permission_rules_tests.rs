//! The person's pattern rules through the real host (card OA8-04, task SE-20 of the OpenCode audit): `data/tools/permissions.json` is read by `load_permission_rules` and given to the policy
//! engine by `build_tool_host`; the rules themselves, how they are matched and what they can do are in `aqua_tool_host::rules`. **They can only ask or refuse, never allow.**

use super::*;
use crate::config_files;
use crate::test_support::{ScriptedApprover, TempWorld};
use aqua_mcp_adapters::browser::BrowserBackend;
use aqua_mcp_adapters::{new_browser_client, new_windows_client};
use aqua_tool_host::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// The real host of `build_tool_host`, over a throw-away data folder, with a workspace folder the file tools trust and, if given, the text of `permissions.json`.
struct World {
    host: Arc<ToolHost>,
    ws: PathBuf,
    data: PathBuf,
    approver: Arc<ScriptedApprover>,
    _cleanup: TempWorld,
}

fn world(name: &str, rules: Option<&str>) -> World {
    let cleanup = TempWorld::new(&format!("permission_rules_{name}"));
    let root = cleanup.root.clone();
    let data = root.join("data");
    let ws = root.join("ws");
    std::fs::create_dir_all(data.join("tools")).unwrap();
    std::fs::create_dir_all(ws.join("secrets")).unwrap();
    std::fs::create_dir_all(ws.join("important")).unwrap();
    let cfg = aqua_native_tools::fs::FsConfig::with_workspace(ws.to_string_lossy().to_string());
    std::fs::write(data.join("tools").join("fs.json"), serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    if let Some(text) = rules {
        std::fs::write(permission_rules_path(&data), text).unwrap();
    }
    let browser = Arc::new(BrowserBackend::new(&root.join("browser"), new_browser_client()));
    let windows = Arc::new(Mutex::new(new_windows_client()));
    let host = build_tool_host(&data, browser, windows);
    let approver = Arc::new(ScriptedApprover::approving(true));
    host.set_approver(approver.clone());
    World { host, ws, data, approver, _cleanup: cleanup }
}

fn call(w: &World, tool: &str, args: Value) -> ToolResult {
    call_in(w, "run_rules", tool, args)
}

fn call_in(w: &World, run: &str, tool: &str, args: Value) -> ToolResult {
    let mut ctx = RequestContext::new(Caller::Agent);
    ctx.agent_run_id = run.into();
    w.host.call_blocking(ToolRequest { tool: tool.into(), arguments: args, context: ctx })
}

fn write(w: &World, relative: &str) -> ToolResult {
    call(w, "fs.write", json!({"path": w.ws.join(relative).to_string_lossy(), "content": "hello"}))
}

#[test]
fn a_deny_rule_beats_full_access() {
    let w = world("deny", Some(r#"{ "rules": [ { "tool": "fs.*", "target_glob": "*/secrets/**", "effect": "deny", "reason": "my secrets folder" } ] }"#));
    w.host.policy().set_full_access(true);
    let ok = write(&w, "ok.txt");
    assert!(ok.ok && w.ws.join("ok.txt").exists(), "outside the rule the write is what it was: {ok:?}");
    assert_eq!(w.approver.asked_count(), 0, "under full access a write in the trusted folder is not asked about");
    for tool_args in [("fs.write", json!({"path": w.ws.join("secrets").join("a.txt").to_string_lossy(), "content": "x"})), ("fs.list", json!({"path": w.ws.join("secrets").to_string_lossy()}))] {
        let res = call(&w, tool_args.0, tool_args.1);
        let error = res.error.as_ref().unwrap_or_else(|| panic!("{} must be refused: {res:?}", tool_args.0));
        assert_eq!(error.code, ErrorCode::PermissionDenied, "{res:?}");
        assert!(error.message.contains("permissions.json") && error.message.contains("my secrets folder"), "{}", error.message);
    }
    assert!(!w.ws.join("secrets").join("a.txt").exists(), "nothing was written");
    assert_eq!(w.approver.asked_count(), 0, "a refusal asks nobody");
}

#[test]
fn an_ask_rule_asks_even_for_a_trusted_root() {
    let w = world("ask", Some(r#"{ "rules": [ { "tool": "fs.write", "target_glob": "*/important/**", "effect": "ask", "reason": "look at what goes in here" } ] }"#));
    w.host.policy().set_full_access(true);
    assert!(write(&w, "plain.txt").ok);
    assert_eq!(w.approver.asked_count(), 0);
    let res = write(&w, "important/a.txt");
    assert!(res.ok, "the person said yes: {res:?}");
    let asked = w.approver.asked();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].why.rule.contains("look at what goes in here") && asked[0].tool == "fs.write", "{:?}", asked[0]);
    // A no stops the write.
    let w = world("ask_no", Some(r#"{ "rules": [ { "tool": "fs.write", "target_glob": "*/important/**", "effect": "ask" } ] }"#));
    w.host.set_approver(Arc::new(ScriptedApprover::denying()));
    w.host.policy().set_full_access(true);
    let res = write(&w, "important/b.txt");
    assert_eq!(res.error.as_ref().map(|e| e.code), Some(ErrorCode::ApprovalDenied), "{res:?}");
    assert!(!w.ws.join("important").join("b.txt").exists());
}

#[test]
fn a_rule_holds_in_a_run_nobody_is_watching_and_the_run_is_told_why() {
    let w = world("background", Some(r#"{ "rules": [ { "tool": "fs.write", "target_glob": "*/important/**", "effect": "ask" } ] }"#));
    let mut ctx = RequestContext::new(Caller::Agent);
    ctx.agent_run_id = "run_bg".into();
    ctx.unattended = true;
    ctx.no_one_to_ask = true;
    let args = json!({"path": w.ws.join("important").join("c.txt").to_string_lossy(), "content": "x"});
    let res = w.host.call_blocking(ToolRequest { tool: "fs.write".into(), arguments: args, context: ctx });
    assert_eq!(res.error.as_ref().map(|e| e.code), Some(ErrorCode::ApprovalDenied), "{res:?}");
    assert!(res.error.unwrap().message.contains("nobody can be asked"));
    assert!(!w.ws.join("important").join("c.txt").exists());
}

#[test]
fn a_file_that_says_allow_is_broken_and_every_change_asks_until_it_is_fixed() {
    let w = world("allow", Some(r#"{ "rules": [ { "tool": "fs.write", "target_glob": "*", "effect": "allow" } ] }"#));
    assert!(w.host.policy().rules().is_broken().is_some_and(|why| why.contains("never allow anything")));
    w.host.policy().set_full_access(true);
    // A read is not held up (it is made in a run of its own: a read of a file or a listing would also make the run it is in count as having read text from outside).
    let read = call_in(&w, "run_reader", "fs.list", json!({"path": w.ws.to_string_lossy()}));
    assert!(read.ok, "a read is not held up: {read:?}");
    assert_eq!(w.approver.asked_count(), 0);
    let res = write(&w, "after.txt");
    assert!(res.ok, "the person said yes: {res:?}");
    let asked = w.approver.asked();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].always_ask && asked[0].why.rule.contains("permissions.json is wrong"), "{:?}", asked[0]);
    // The status names the file and the mistake, and says what it does meanwhile.
    let report = config_files::status(&w.data).into_iter().find(|r| r.name == "permissions.json").expect("in the status");
    assert_eq!(report.state, config_files::ConfigState::Broken);
    assert!(report.effect.as_deref().is_some_and(|e| e.contains("every call that is not a read asks")), "{:?}", report.effect);
}

#[test]
fn no_file_means_no_rules_and_nothing_is_written() {
    let w = world("none", None);
    assert!(w.host.policy().rules().is_empty());
    assert!(!permission_rules_path(&w.data).exists(), "the person makes the file; the core never writes one");
    w.host.policy().set_full_access(true);
    assert!(write(&w, "secrets/a.txt").ok);
    let report = config_files::status(&w.data).into_iter().find(|r| r.name == "permissions.json").unwrap();
    assert_eq!(report.state, config_files::ConfigState::Absent);
}

#[test]
fn comments_and_a_trailing_comma_are_read_like_in_every_other_config_file() {
    let d = TempWorld::new("permission_rules_jsonc");
    std::fs::create_dir_all(d.root.join("tools")).unwrap();
    std::fs::write(permission_rules_path(&d.root), "\u{feff}{\n  // never touch the tax folder\n  \"rules\": [\n    { \"tool\": \"fs.*\", \"target_glob\": \"*/tax/**\", \"effect\": \"deny\", }, /* and ask about pushes */\n    { \"tool\": \"term.*\", \"target_glob\": \"*git* push*\", \"effect\": \"ask\" },\n  ],\n}\n").unwrap();
    let rules = load_permission_rules(&d.root);
    assert_eq!((rules.len(), rules.is_broken()), (2, None), "{rules:?}");
}

#[test]
fn install_gives_the_policy_the_rules_and_says_how_many() {
    let d = TempWorld::new("permission_rules_install");
    std::fs::create_dir_all(d.root.join("tools")).unwrap();
    let policy = PolicyEngine::new(Mode::Standard);
    assert_eq!(install_permission_rules(&policy, &d.root), 0);
    std::fs::write(permission_rules_path(&d.root), r#"{ "rules": [ { "tool": "http.*", "target_glob": "*.example.com:*", "effect": "always_ask" } ] }"#).unwrap();
    assert_eq!(install_permission_rules(&policy, &d.root), 1);
    assert_eq!(policy.rules().rules()[0].effect, RuleEffect::AlwaysAsk);
    std::fs::write(permission_rules_path(&d.root), "{ nope").unwrap();
    assert_eq!(install_permission_rules(&policy, &d.root), 0, "a file that cannot be used has no rules of its own");
    assert!(policy.rules().is_broken().is_some(), "and asks about every change instead");
}
