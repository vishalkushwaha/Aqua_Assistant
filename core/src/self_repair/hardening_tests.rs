//! Tests of the hardening of the first Phase 7 round (agent J, round 5): the undo of an allowed folder through the real file tools, a switch of platform only where one step posts,
//! and the report's "no undo" list once a grant can be taken back. (The first round's tests are in `tests.rs`, `report.rs` and `gaps.rs`.)
//!
//! Nothing here touches process-wide state: each test builds its own stores, folders and tool host.

use super::proposals::{approve, create_proposals_for, revoke_on, undo, Change, Env, FolderGrant, FolderHost};
use super::report::{self, ReportInput};
use super::store::{Gap, GapKind, NewGap, NewProposal, RepairError, RepairStore};
use super::test_support::temp_dir;
use crate::flows::model::{Edge, Flow};
use crate::flows::store::FlowStore;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const T: i64 = 1_800_000_000;

struct Rig {
    store: RepairStore,
    flows: FlowStore,
}

fn rig(name: &str) -> Rig {
    let dir = temp_dir(name);
    Rig { store: RepairStore::new(dir.join("self_repair.db")), flows: FlowStore::new(dir.join("flows.db")) }
}

fn env<'a>(r: &'a Rig, folders: &'a dyn FolderHost, now: i64) -> Env<'a> {
    Env { flows: &r.flows, folders, owner: "local", now }
}

/// A host that is asked nothing in these tests (the flow proposals never touch a folder).
struct NoFolders;

impl FolderHost for NoFolders {
    fn allow(&self, _path: &str, _access: &str) -> Result<FolderGrant, String> {
        Err("not asked in this test".into())
    }
}

// ---- a switch of platform only where one step posts -------------------------------------------------------

/// The social-post template pointed at Facebook (the platform that is not built). With `second`, a second step posts to Facebook as well.
fn facebook_flow(flows: &FlowStore, second: bool) -> Flow {
    let mut def = crate::flows::templates::build("social_post").expect("the template");
    let post = def.node_mut("publish").expect("the post step");
    post.name = "Publish to Facebook".into();
    post.config["platform"] = json!("facebook");
    post.credential = Some("facebook/main".into());
    let site = def.node_mut("open_site").expect("the site step");
    site.config["url"] = json!("https://www.facebook.com");
    site.config["session"] = json!("facebook-main");
    if second {
        let mut again = def.node("publish").expect("the post step").clone();
        again.id = "publish_again".into();
        again.name = "Publish to Facebook again".into();
        def.nodes.push(again);
        def.edges.push(Edge::new("publish", "publish_again", "out"));
    }
    flows.create("local", def, "user", "", T).unwrap()
}

fn platform_gap(store: &RepairStore, flow: &Flow) -> Gap {
    let mut g = NewGap::new(GapKind::PlatformNotBuilt, format!("{}:publish:facebook", flow.id), "Posting to Facebook is not built yet");
    g.flow_id = Some(flow.id);
    g.node_id = "publish".into();
    g.data = json!({ "platform": "facebook", "node_id": "publish", "flow_id": flow.id });
    store.record(g, T).unwrap()
}

#[test]
fn a_flow_that_posts_to_the_platform_in_more_than_one_place_is_offered_no_switch_and_a_switch_made_for_it_is_refused_with_the_reason() {
    let r = rig("h_several");
    let flow = facebook_flow(&r.flows, true);
    let gap = platform_gap(&r.store, &flow);
    let made = create_proposals_for(&r.store, &env(&r, &NoFolders, T), gap.id).unwrap();
    let kinds: Vec<&str> = made.proposals.iter().map(|p| p.kind.as_str()).collect();
    assert_eq!(kinds, ["switch_off_step"], "no switch is offered, and the switch-off of the one step still is: {}", made.note);
    assert_eq!(made.note, "No switch to another platform is offered: this flow posts to Facebook in more than one place: switch it from the flow editor.");
    // A switch proposed for this flow anyway (by hand, or by the older rule) is refused when it is approved, and the flow is as it was.
    let stale = r
        .store
        .proposal_add(
            NewProposal {
                gap_id: gap.id,
                kind: "switch_platform".into(),
                title: "t".into(),
                reason: "r".into(),
                dry_run: "d".into(),
                payload: serde_json::to_value(Change::SwitchPlatform { flow_id: flow.id, node_id: "publish".into(), from: "facebook".into(), to: "linkedin".into(), flow_version: 1 }).unwrap(),
            },
            T,
        )
        .unwrap();
    let out = approve(&r.store, &env(&r, &NoFolders, T + 1), stale.id, "ui").unwrap();
    assert_eq!(out.status, "failed", "{out:?}");
    assert!(out.error.contains("posts to Facebook in more than one place: switch it from the flow editor"), "{}", out.error);
    let after = r.flows.get("local", flow.id).unwrap().unwrap();
    assert_eq!(after.version, 1, "nothing was written");
    assert_eq!((after.def.node("publish").unwrap().config["platform"].clone(), after.def.node("publish_again").unwrap().config["platform"].clone()), (json!("facebook"), json!("facebook")));
}

#[test]
fn a_flow_with_one_posting_step_is_still_offered_the_switch_and_there_is_nothing_to_explain() {
    let r = rig("h_single");
    let flow = facebook_flow(&r.flows, false);
    let gap = platform_gap(&r.store, &flow);
    let made = create_proposals_for(&r.store, &env(&r, &NoFolders, T), gap.id).unwrap();
    let kinds: Vec<&str> = made.proposals.iter().map(|p| p.kind.as_str()).collect();
    assert_eq!(kinds, ["switch_platform", "switch_off_step"]);
    assert_eq!(made.note, "");
    // The words of the refusal are one sentence, with the label of the platform.
    assert_eq!(super::proposals::several_places("LinkedIn"), "this flow posts to LinkedIn in more than one place: switch it from the flow editor");
}

// ---- the undo of an allowed folder, through the real file tools --------------------------------------------

/// Says yes to every question and counts them.
struct Yes(Arc<AtomicUsize>);

impl aqua_tool_host::Approver for Yes {
    fn request(&self, _req: aqua_tool_host::ApprovalRequest) -> aqua_tool_host::ProviderFuture<'_, aqua_tool_host::ApprovalOutcome> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { aqua_tool_host::ApprovalOutcome::Approved(aqua_tool_host::ApprovalScope::Once) })
    }
}

/// The tool host as the proposals see it (the real file tools in a temporary folder, a person who says yes), as `HostFolders` calls it: allowing goes through `fs.allow_folder` as the
/// app, and taking back is `revoke_on`, the same function `HostFolders::revoke` uses on the running host.
struct Live {
    host: aqua_tool_host::ToolHost,
    guard: Arc<aqua_native_tools::fs::PathGuard>,
    asked: Arc<AtomicUsize>,
    dir: PathBuf,
}

fn live(name: &str) -> Live {
    use aqua_native_tools::fs::{Access, FsConfig, FsProvider, PathGuard, Root, Trash};
    let dir = temp_dir(name);
    for d in ["ws", "outside", "aqua/tools"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let cfg = FsConfig { roots: vec![Root { alias: "ws".into(), path: dir.join("ws").to_string_lossy().into(), access: Access::Rw, trusted: true }], ..FsConfig::with_workspace("unused") };
    let guard = Arc::new(PathGuard::new(cfg, vec![dir.join("aqua")]).unwrap());
    // The temporary folder of a test is inside AppData, which the folder rules keep apart.
    guard.set_kept(vec![]);
    let trash = Arc::new(Trash::new(dir.join("aqua").join("trash")).unwrap());
    let host = aqua_tool_host::ToolHost::new(Arc::new(aqua_tool_host::PolicyEngine::new(aqua_tool_host::Mode::Standard)), Arc::new(aqua_tool_host::AuditLedger::open(dir.join("aqua/tools/audit.jsonl"))));
    let asked = Arc::new(AtomicUsize::new(0));
    host.set_approver(Arc::new(Yes(asked.clone())));
    host.register(Arc::new(FsProvider::new(guard.clone(), trash))).unwrap();
    Live { host, guard, asked, dir }
}

impl Live {
    fn call(&self, tool: &str, args: serde_json::Value) -> aqua_tool_host::ToolResult {
        let mut ctx = aqua_tool_host::RequestContext::new(aqua_tool_host::Caller::Agent);
        ctx.agent_run_id = "run_h".into();
        self.host.call_blocking(aqua_tool_host::ToolRequest { tool: tool.into(), arguments: args, context: ctx })
    }

    fn write_in_outside(&self, name: &str) -> aqua_tool_host::ToolResult {
        self.call("fs.write", json!({ "path": format!("{}\\{name}", self.dir.join("outside").display()), "content": "x" }))
    }
}

impl FolderHost for Live {
    fn allow(&self, path: &str, access: &str) -> Result<FolderGrant, String> {
        let context = aqua_tool_host::RequestContext::new(aqua_tool_host::Caller::Ui);
        let res = self.host.call_blocking(aqua_tool_host::ToolRequest { tool: "fs.allow_folder".into(), arguments: json!({ "path": path, "access": access }), context });
        let (decision, by) = res.approval.as_ref().map(|a| (a.decision.clone(), a.by.clone())).unwrap_or_default();
        if res.ok {
            Ok(FolderGrant { text: res.text.clone(), request_id: res.request_id.clone(), decision, by, added: res.data["added"].as_bool().unwrap_or(true) })
        } else {
            Err(format!("the tool host did not allow the folder: {}", res.text))
        }
    }

    fn revoke(&self, path: &str) -> Result<String, String> {
        revoke_on(&self.host, path)
    }

    fn can_revoke(&self) -> bool {
        true
    }
}

fn folder_gap(store: &RepairStore, folder: &str) -> Gap {
    let mut g = NewGap::new(GapKind::FolderNotAllowed, folder.to_ascii_lowercase(), format!("The folder {folder} is outside the folders Aqua may use"));
    g.data = json!({ "folder": folder, "access": "rw", "named_by_person": true });
    store.record(g, T).unwrap()
}

#[test]
fn the_undo_of_an_allowed_folder_takes_the_grant_back_through_the_real_tool_host_and_the_file_tools_lose_the_folder() {
    let r = rig("h_undo_folder");
    let live = live("h_undo_folder_host");
    let outside = live.dir.join("outside").to_string_lossy().to_string();
    let gap = folder_gap(&r.store, &outside);
    let p = create_proposals_for(&r.store, &env(&r, &live, T), gap.id).unwrap().proposals.remove(0);
    assert_eq!(p.kind, "allow_folder");
    // Approving asks the person once, through the tool host, and the folder is usable (every change in it asks again).
    let done = approve(&r.store, &env(&r, &live, T + 1), p.id, "ui").unwrap();
    assert_eq!(done.status, "applied", "{done:?}");
    assert_eq!(done.undo["kind"], "revoke_folder");
    assert_eq!(live.asked.load(Ordering::SeqCst), 1);
    let written = live.write_in_outside("hello.py");
    assert!(written.ok, "{}", written.text);
    assert_eq!(live.asked.load(Ordering::SeqCst), 2, "the change in the allowed folder asked");
    assert_eq!(live.guard.granted_folders().len(), 1);
    // The undo takes the grant back and asks nothing: the folder is outside the folders Aqua may use again, and what was written stays.
    let undone = undo(&r.store, &env(&r, &live, T + 2), p.id).unwrap();
    assert_eq!(undone.status, "undone", "{undone:?}");
    assert!(undone.result["undo"].as_str().unwrap_or("").starts_with("Took back "), "{}", undone.result);
    assert_eq!(live.asked.load(Ordering::SeqCst), 2, "taking access away asked nothing");
    assert!(live.guard.granted_folders().is_empty());
    let after = live.write_in_outside("again.py");
    assert!(!after.ok && after.text.contains("outside the folders Aqua may use"), "{}", after.text);
    assert!(live.dir.join("outside").join("hello.py").exists() && !live.dir.join("outside").join("again.py").exists());
    assert_eq!(r.store.gap(gap.id).unwrap().unwrap().status, "new", "the problem is open again");
    // Once is enough.
    assert!(matches!(undo(&r.store, &env(&r, &live, T + 3), p.id), Err(RepairError::Refused(m)) if m.contains("undone")));
}

#[test]
fn an_undo_after_the_grant_is_already_gone_succeeds_and_says_so_and_a_folder_of_the_settings_is_never_taken_back() {
    let r = rig("h_undo_gone");
    let live = live("h_undo_gone_host");
    let outside = live.dir.join("outside").to_string_lossy().to_string();
    let gap = folder_gap(&r.store, &outside);
    let p = create_proposals_for(&r.store, &env(&r, &live, T), gap.id).unwrap().proposals.remove(0);
    assert_eq!(approve(&r.store, &env(&r, &live, T + 1), p.id, "ui").unwrap().status, "applied");
    // Aqua restarted in between (a restart forgets every grant), or the grant was taken back by hand: the folder is not allowed, which is what the undo wants.
    assert!(live.guard.revoke_folder("@outside").unwrap());
    let undone = undo(&r.store, &env(&r, &live, T + 2), p.id).unwrap();
    assert_eq!(undone.status, "undone", "{undone:?}");
    assert!(undone.result["undo"].as_str().unwrap_or("").contains("not allowed any more"), "{}", undone.result);
    // A folder the settings give is refused by the tool, whatever asks; the host that has no such tool is an error too, not a success.
    let said = revoke_on(&live.host, &live.dir.join("ws").to_string_lossy()).unwrap_err();
    assert!(said.contains("did not take the folder back") && said.contains("settings"), "{said}");
    let bare = aqua_tool_host::ToolHost::new(Arc::new(aqua_tool_host::PolicyEngine::new(aqua_tool_host::Mode::Standard)), Arc::new(aqua_tool_host::AuditLedger::open(live.dir.join("bare.jsonl"))));
    let said = revoke_on(&bare, "E:/work").unwrap_err();
    assert!(said.contains("did not take the folder back") && said.contains("unknown tool"), "{said}");
}

// ---- the report once a grant can be taken back --------------------------------------------------------------

#[test]
fn a_folder_grant_an_agent_run_asked_for_is_listed_as_having_no_undo_only_while_the_host_cannot_take_it_back() {
    const NOW: i64 = 2_000_000_000;
    let dir = temp_dir("h_report");
    let line = json!({
        "seq": 1, "ts_unix_ms": (NOW - 2 * 86_400) * 1000, "request_id": "r1", "agent_run_id": "run_g", "caller": "agent", "tool": "fs.allow_folder",
        "args": { "path": "E:/work" }, "decision": "ask", "classes": ["PRIVILEGED"], "ok": true, "error_code": null,
    })
    .to_string();
    let log = dir.join("host.audit.jsonl");
    std::fs::write(&log, format!("{line}\n")).unwrap();
    let store = RepairStore::new(dir.join("self_repair.db"));
    let none = |_: &str| false;
    let build = |can: bool| report::build(&store, ReportInput { audit_path: &log, days: 7, now: NOW, has_undo: &none, folders_can_be_taken_back: can, verify_chain: None }).unwrap();
    let before = build(false);
    let items = before["changes_without_undo"]["items"].as_array().unwrap();
    assert_eq!((items.len(), items[0]["tool"].clone(), before["changes_with_undo"]["calls"].clone()), (1, json!("fs.allow_folder"), json!(0)), "{before}");
    assert!(items[0]["why"].as_str().unwrap().contains("cannot be taken back sooner"), "{}", items[0]);
    // With fs.revoke_folder the grant has an undo: it is counted with the changes that can be put back, and not listed in red.
    let after = build(true);
    assert_eq!((after["changes_without_undo"]["items"].as_array().unwrap().len(), after["changes_without_undo"]["count"].clone(), after["changes_with_undo"]["calls"].clone()), (0, json!(0), json!(1)), "{after}");
    assert!(after["text"].as_str().unwrap().contains("Changes with no undo: none"), "{}", after["text"]);
}
