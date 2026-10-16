//! Tests of the self-repair store, proposals and the places that notice a gap. (The report has its own tests in `report.rs`, the gap texts in `gaps.rs`.)

use super::proposals::{approve, create_proposals_for, dismiss_gap, reject, undo, Env, FolderGrant, FolderHost};
use super::store::{Gap, GapKind, NewGap, NewProposal, RepairError, RepairStore};
use super::test_support::{global, temp_dir, unique};
use crate::flows::model::Flow;
use crate::flows::store::FlowStore;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const T: i64 = 1_800_000_000;

// ---- the store ---------------------------------------------------------------------------------------------

#[test]
fn the_same_gap_again_is_one_row_whose_count_grows_and_a_dismissed_gap_stays_dismissed() {
    let store = RepairStore::new(temp_dir("store_dedup").join("self_repair.db"));
    let mut g = NewGap::new(GapKind::FolderNotAllowed, "e:/work", "The folder E:/work is outside the folders Aqua may use");
    g.asked = "write a file in E:/work".into();
    g.run_id = "run_1".into();
    g.data = json!({ "folder": "E:/work", "access": "ro", "named_by_person": false, "refused_by_guard": true });
    let first = store.record(g.clone(), T).unwrap();
    assert_eq!((first.count, first.at, first.last_at, first.status.as_str()), (1, T, T, "new"));
    // The second time: a write is needed, the person named the folder, and the person (not the guard) said no. No question was asked, no run is known.
    g.run_id = String::new();
    g.asked = String::new();
    g.data = json!({ "folder": "E:/work", "access": "rw", "named_by_person": true, "refused_by_guard": false });
    let second = store.record(g.clone(), T + 60).unwrap();
    assert_eq!((second.id, second.count, second.at, second.last_at), (first.id, 2, T, T + 60), "one row, a growing count, the first time kept");
    assert_eq!(store.gaps(None, 100).unwrap().len(), 1);
    assert_eq!((second.asked.as_str(), second.run_id.as_str()), ("write a file in E:/work", "run_1"), "what a later report does not know is kept");
    assert_eq!((second.data["access"].clone(), second.data["named_by_person"].clone(), second.data["refused_by_guard"].clone()), (json!("rw"), json!(true), json!(false)));
    // Another report that does not know who said no leaves what is known; and a named folder does not become unnamed.
    g.data = json!({ "folder": "E:/work", "access": "ro", "named_by_person": false });
    let third = store.record(g.clone(), T + 120).unwrap();
    assert_eq!((third.data["access"].clone(), third.data["named_by_person"].clone(), third.data["refused_by_guard"].clone()), (json!("rw"), json!(true), json!(false)));
    // A different kind with the same key is another gap.
    let other = store.record(NewGap::new(GapKind::Other, "e:/work", "something else"), T).unwrap();
    assert_ne!(other.id, first.id);
    // Dismissed stays dismissed while the count grows; a fixed gap that comes back is open again.
    store.set_gap_status(first.id, "dismissed").unwrap();
    let back = store.record(g.clone(), T + 180).unwrap();
    assert_eq!((back.status.as_str(), back.count), ("dismissed", 4));
    store.set_gap_status(first.id, "fixed").unwrap();
    let back = store.record(g, T + 240).unwrap();
    assert_eq!((back.status.as_str(), back.count), ("new", 5), "a fix that did not hold");
    assert!(store.set_gap_status(first.id, "bogus").is_err());
    assert!(matches!(store.gaps(Some("bogus"), 10), Err(RepairError::Refused(_))));
    assert_eq!(store.gaps(Some("open"), 10).unwrap().len(), 2);
    assert_eq!(store.gaps(Some("dismissed"), 10).unwrap().len(), 0);
}

#[test]
fn the_table_is_pruned_dismissed_and_fixed_gaps_first_and_a_reopened_database_keeps_its_rows() {
    let dir = temp_dir("store_prune");
    let store = RepairStore::new(dir.join("self_repair.db"));
    for i in 0..505 {
        store.record(NewGap::new(GapKind::Other, format!("k{i}"), format!("gap {i}")), T + i).unwrap();
    }
    assert_eq!(store.gaps(None, 500).unwrap().len(), 500, "the list is capped at 500 rows per answer");
    let all = |s: &RepairStore| s.gaps_since(0).unwrap().len();
    assert_eq!(all(&store), 500, "gaps_since is capped the same way");
    // Nothing was dismissed, so nothing was dropped: 505 rows are there (an open gap is never pruned while the table is under its hard cap).
    let conn = rusqlite::Connection::open(store.path()).unwrap();
    let rows = |c: &rusqlite::Connection| c.query_row("SELECT COUNT(*) FROM gaps", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(rows(&conn), 505);
    // Ten are dismissed; the next gap takes the table to 506 and the oldest dismissed ones go until 500 remain.
    for i in 0..10 {
        let id = store.gaps(None, 500).unwrap().into_iter().find(|g| g.key == format!("k{i}")).map(|g| g.id);
        // k0..k4 are older than the newest 500 rows the list shows; look them up straight from the table.
        let id = id.unwrap_or_else(|| conn.query_row("SELECT id FROM gaps WHERE gap_key = ?1", [format!("k{i}")], |r| r.get(0)).unwrap());
        store.set_gap_status(id, "dismissed").unwrap();
    }
    store.record(NewGap::new(GapKind::Other, "newest", "the newest gap"), T + 1000).unwrap();
    assert_eq!(rows(&conn), 500);
    let kept: i64 = conn.query_row("SELECT COUNT(*) FROM gaps WHERE status = 'dismissed'", [], |r| r.get(0)).unwrap();
    assert_eq!(kept, 4, "six of the ten dismissed gaps were dropped, the oldest first");
    let gone: i64 = conn.query_row("SELECT COUNT(*) FROM gaps WHERE gap_key IN ('k0','k1','k2','k3','k4','k5')", [], |r| r.get(0)).unwrap();
    assert_eq!(gone, 0);
    // A new handle on the same file sees the rows, and the migration does not run twice.
    drop(conn);
    let again = RepairStore::new(dir.join("self_repair.db"));
    assert!(again.gaps(None, 10).unwrap().iter().any(|g| g.key == "newest"));
    let version: i64 = rusqlite::Connection::open(again.path()).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version as usize, super::store::MIGRATIONS.len());
}

#[test]
fn a_proposal_is_taken_for_approval_by_exactly_one_caller() {
    let store = RepairStore::new(temp_dir("store_claim").join("self_repair.db"));
    let gap = store.record(NewGap::new(GapKind::Other, "x", "x"), T).unwrap();
    let p = store.proposal_add(NewProposal { gap_id: gap.id, kind: "allow_folder".into(), title: "t".into(), reason: "r".into(), dry_run: "d".into(), payload: json!({ "kind": "allow_folder" }) }, T).unwrap();
    assert_eq!(p.status, "pending");
    assert!(store.proposal_claim(p.id, "ui", T).unwrap());
    assert!(!store.proposal_claim(p.id, "ui", T).unwrap(), "the second caller finds it taken");
    assert!(!store.proposal_reject(p.id, "no").unwrap(), "an approved proposal cannot be rejected");
    assert_eq!(store.proposal(p.id).unwrap().unwrap().approved_by, "ui");
}

// ---- proposals ---------------------------------------------------------------------------------------------

/// The tool host's folders, as a person would answer them: yes (the folder is added, or was there already) or no (with the host's words).
struct FakeFolders {
    answer: Result<bool, String>,
    can_revoke: bool,
    asked: Mutex<Vec<(String, String)>>,
    revoked: Mutex<Vec<String>>,
}

impl FakeFolders {
    fn new(answer: Result<bool, String>, can_revoke: bool) -> Self {
        Self { answer, can_revoke, asked: Mutex::new(Vec::new()), revoked: Mutex::new(Vec::new()) }
    }
}

impl FolderHost for FakeFolders {
    fn allow(&self, path: &str, access: &str) -> Result<FolderGrant, String> {
        self.asked.lock().unwrap().push((path.into(), access.into()));
        match &self.answer {
            Ok(added) => Ok(FolderGrant { text: format!("Allowed {path}"), request_id: "req_fake".into(), decision: "approved".into(), by: "ui".into(), added: *added }),
            Err(why) => Err(why.clone()),
        }
    }
    fn revoke(&self, path: &str) -> Result<String, String> {
        if !self.can_revoke {
            return Err("the tool host cannot take a folder back before Aqua restarts (a grant lasts until then)".into());
        }
        self.revoked.lock().unwrap().push(path.into());
        Ok(format!("Took back {path}"))
    }
}

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

/// The social-post template, pointed at Facebook (the platform that is not built): the post step and the page it opens.
fn facebook_flow(flows: &FlowStore) -> Flow {
    let mut def = crate::flows::templates::build("social_post").expect("the template");
    let post = def.node_mut("publish").expect("the post step");
    post.name = "Publish to Facebook".into();
    post.config["platform"] = json!("facebook");
    post.credential = Some("facebook/main".into());
    let site = def.node_mut("open_site").expect("the site step");
    site.config["url"] = json!("https://www.facebook.com");
    site.config["session"] = json!("facebook-main");
    flows.create("local", def, "user", "", T).unwrap()
}

fn platform_gap(store: &RepairStore, flow: &Flow) -> Gap {
    let mut g = NewGap::new(GapKind::PlatformNotBuilt, format!("{}:publish:facebook", flow.id), "Posting to Facebook is not built yet");
    g.flow_id = Some(flow.id);
    g.node_id = "publish".into();
    g.data = json!({ "platform": "facebook", "node_id": "publish", "flow_id": flow.id });
    store.record(g, T).unwrap()
}

fn folder_gap(store: &RepairStore, folder: &str, extra: Value) -> Gap {
    let mut g = NewGap::new(GapKind::FolderNotAllowed, folder.to_ascii_lowercase(), format!("The folder {folder} is outside the folders Aqua may use"));
    let mut d = json!({ "folder": folder, "access": "rw" });
    for (k, v) in extra.as_object().unwrap() {
        d[k] = v.clone();
    }
    g.data = d;
    store.record(g, T).unwrap()
}

fn version_of(r: &Rig, flow: &Flow) -> i64 {
    r.flows.get("local", flow.id).unwrap().unwrap().version
}

#[test]
fn a_platform_that_is_not_built_gives_a_switch_and_a_switch_off_and_asking_again_gives_the_same_ones() {
    let r = rig("p_make");
    let flow = facebook_flow(&r.flows);
    let gap = platform_gap(&r.store, &flow);
    let folders = FakeFolders::new(Ok(true), false);
    let made = create_proposals_for(&r.store, &env(&r, &folders, T), gap.id).unwrap();
    let kinds: Vec<&str> = made.proposals.iter().map(|p| p.kind.as_str()).collect();
    assert_eq!(kinds, ["switch_platform", "switch_off_step"], "{} / {}", made.note, made.proposals.len());
    let switch = &made.proposals[0];
    assert!(switch.title.contains("LinkedIn instead of Facebook"), "{}", switch.title);
    assert!(switch.reason.starts_with("Posting to Facebook is not built yet"), "{}", switch.reason);
    assert!(switch.dry_run.contains("posts to LinkedIn instead of Facebook") && switch.dry_run.contains("version 1"), "{}", switch.dry_run);
    assert!(made.proposals[1].dry_run.contains("switch off the step \"Publish to Facebook\""), "{}", made.proposals[1].dry_run);
    assert!(made.proposals.iter().all(|p| p.status == "pending" && p.undo.is_null()), "nothing is applied, and no undo exists yet");
    assert_eq!(r.store.gap(gap.id).unwrap().unwrap().status, "proposed");
    assert_eq!(version_of(&r, &flow), 1, "making a proposal changes nothing");
    let again = create_proposals_for(&r.store, &env(&r, &folders, T + 1), gap.id).unwrap();
    assert_eq!(again.proposals.iter().map(|p| p.id).collect::<Vec<_>>(), made.proposals.iter().map(|p| p.id).collect::<Vec<_>>());
    assert_eq!(r.store.proposals(None, Some(gap.id), 50).unwrap().len(), 2);
    // A gap of a kind with no fix offers none, and a gap whose flow is gone says so.
    let other = r.store.record(NewGap::new(GapKind::CredentialMissing, "linkedin/main", "No account saved for LinkedIn"), T).unwrap();
    let none = create_proposals_for(&r.store, &env(&r, &folders, T), other.id).unwrap();
    assert!(none.proposals.is_empty() && none.note.contains("no fix to offer"), "{}", none.note);
    assert!(r.flows.delete("local", flow.id, "user", T).unwrap());
    assert!(matches!(create_proposals_for(&r.store, &env(&r, &folders, T), gap.id), Err(RepairError::Refused(m)) if m.contains("flow is gone")));
}

#[test]
fn approving_a_switch_makes_one_new_version_by_self_repair_and_undo_brings_the_old_one_back() {
    let r = rig("p_switch");
    let flow = facebook_flow(&r.flows);
    let gap = platform_gap(&r.store, &flow);
    let folders = FakeFolders::new(Ok(true), false);
    let made = create_proposals_for(&r.store, &env(&r, &folders, T), gap.id).unwrap();
    let (switch, off) = (made.proposals[0].id, made.proposals[1].id);
    let done = approve(&r.store, &env(&r, &folders, T + 10), switch, "ui").unwrap();
    assert_eq!((done.status.as_str(), done.approved_by.as_str(), done.applied_at), ("applied", "ui", Some(T + 10)), "{done:?}");
    assert_eq!(done.undo, json!({ "kind": "flow_version", "flow_id": flow.id, "restore_version": 1, "applied_version": 2 }));
    let after = r.flows.get("local", flow.id).unwrap().unwrap();
    assert_eq!(after.version, 2);
    assert_eq!(after.def.node("publish").unwrap().config["platform"], "linkedin");
    assert!(after.def.node("open_site").unwrap().config["url"].as_str().unwrap().contains("linkedin"), "{}", after.def.node("open_site").unwrap().config);
    let versions = r.flows.versions("local", flow.id).unwrap();
    assert_eq!((versions[0].version, versions[0].author.as_str()), (2, "self_repair"));
    assert_eq!(r.store.gap(gap.id).unwrap().unwrap().status, "fixed");
    // The same proposal a second time is refused, and nothing changes.
    assert!(matches!(approve(&r.store, &env(&r, &folders, T + 11), switch, "ui"), Err(RepairError::Refused(m)) if m.contains("applied")));
    assert_eq!(version_of(&r, &flow), 2);
    // The other proposal was made for version 1: the flow has moved on, so it is refused with the reason and nothing is forced.
    let stale = approve(&r.store, &env(&r, &folders, T + 12), off, "ui").unwrap();
    assert_eq!(stale.status, "failed");
    assert!(stale.error.contains("moved on") && stale.error.contains("version 2"), "{}", stale.error);
    assert_eq!(version_of(&r, &flow), 2);
    // Undo restores version 1 as a new version, written by self_repair.
    let undone = undo(&r.store, &env(&r, &folders, T + 20), switch).unwrap();
    assert_eq!((undone.status.as_str(), undone.undone_at), ("undone", Some(T + 20)));
    let back = r.flows.get("local", flow.id).unwrap().unwrap();
    assert_eq!(back.version, 3);
    assert_eq!(back.def.node("publish").unwrap().config["platform"], "facebook");
    assert_eq!(back.def.node("publish").unwrap().name, "Publish to Facebook");
    let versions = r.flows.versions("local", flow.id).unwrap();
    assert_eq!((versions[0].author.as_str(), versions[0].note.as_str()), ("self_repair", "restored version 1"));
    assert_eq!(r.store.gap(gap.id).unwrap().unwrap().status, "new", "the problem is open again");
    assert!(matches!(undo(&r.store, &env(&r, &folders, T + 21), switch), Err(RepairError::Refused(m)) if m.contains("undone")));
}

#[test]
fn switching_a_step_off_and_undoing_it_and_an_undo_after_someone_else_edited_the_flow() {
    let r = rig("p_off");
    let flow = facebook_flow(&r.flows);
    let gap = platform_gap(&r.store, &flow);
    let folders = FakeFolders::new(Ok(true), false);
    let made = create_proposals_for(&r.store, &env(&r, &folders, T), gap.id).unwrap();
    let off = made.proposals.iter().find(|p| p.kind == "switch_off_step").unwrap().id;
    let done = approve(&r.store, &env(&r, &folders, T + 1), off, "ui").unwrap();
    assert_eq!(done.status, "applied", "{done:?}");
    assert!(!r.flows.get("local", flow.id).unwrap().unwrap().def.node("publish").unwrap().enabled);
    assert_eq!(version_of(&r, &flow), 2);
    // A person edits the flow after that: undoing would throw their edit away, so it is not done.
    let mut edited = r.flows.get("local", flow.id).unwrap().unwrap().def;
    edited.description = "changed by a person".into();
    r.flows.update("local", flow.id, edited, "user", "edit", Some(2), T + 2).unwrap().unwrap();
    let refused = undo(&r.store, &env(&r, &folders, T + 3), off).unwrap_err();
    assert!(matches!(&refused, RepairError::Refused(m) if m.contains("edited since")), "{refused}");
    assert_eq!(r.store.proposal(off).unwrap().unwrap().status, "applied", "a refused undo leaves the proposal as it was");
    assert_eq!(version_of(&r, &flow), 3);
    // Without the edit in between, the undo switches the step back on.
    let r2 = rig("p_off2");
    let flow2 = facebook_flow(&r2.flows);
    let gap2 = platform_gap(&r2.store, &flow2);
    let made2 = create_proposals_for(&r2.store, &env(&r2, &folders, T), gap2.id).unwrap();
    let off2 = made2.proposals.iter().find(|p| p.kind == "switch_off_step").unwrap().id;
    approve(&r2.store, &env(&r2, &folders, T + 1), off2, "ui").unwrap();
    undo(&r2.store, &env(&r2, &folders, T + 2), off2).unwrap();
    assert!(r2.flows.get("local", flow2.id).unwrap().unwrap().def.node("publish").unwrap().enabled);
    assert_eq!(version_of(&r2, &flow2), 3);
}

#[test]
fn a_flow_that_was_edited_after_the_proposal_is_refused_with_the_reason_and_nothing_is_forced() {
    let r = rig("p_moved");
    let flow = facebook_flow(&r.flows);
    let gap = platform_gap(&r.store, &flow);
    let folders = FakeFolders::new(Ok(true), false);
    let made = create_proposals_for(&r.store, &env(&r, &folders, T), gap.id).unwrap();
    let mut edited = flow.def.clone();
    edited.description = "changed by a person".into();
    r.flows.update("local", flow.id, edited, "user", "edit", Some(1), T + 5).unwrap().unwrap();
    for p in &made.proposals {
        let out = approve(&r.store, &env(&r, &folders, T + 6), p.id, "ui").unwrap();
        assert_eq!(out.status, "failed", "{}", p.kind);
        assert!(out.error.contains("moved on"), "{}", out.error);
    }
    assert_eq!(version_of(&r, &flow), 2, "the person's version is the newest, and the flow is as they left it");
    assert_eq!(r.flows.get("local", flow.id).unwrap().unwrap().def.node("publish").unwrap().config["platform"], "facebook");
    // Asking again makes new proposals for the new version, and the failed ones are not offered again.
    let fresh = create_proposals_for(&r.store, &env(&r, &folders, T + 7), gap.id).unwrap();
    assert_eq!(fresh.proposals.len(), 2);
    assert!(fresh.proposals.iter().all(|p| made.proposals.iter().all(|old| old.id != p.id) && p.payload["flow_version"] == 2));
}

#[test]
fn rejecting_changes_nothing_and_dismissing_a_gap_rejects_what_waited_for_it() {
    let r = rig("p_reject");
    let flow = facebook_flow(&r.flows);
    let gap = platform_gap(&r.store, &flow);
    let folders = FakeFolders::new(Ok(true), false);
    let made = create_proposals_for(&r.store, &env(&r, &folders, T), gap.id).unwrap();
    let no = reject(&r.store, made.proposals[0].id).unwrap();
    assert_eq!(no.status, "rejected");
    assert_eq!(version_of(&r, &flow), 1, "rejecting changes nothing");
    assert!(matches!(reject(&r.store, made.proposals[0].id), Err(RepairError::Refused(_))), "only a pending proposal can be rejected");
    assert!(matches!(approve(&r.store, &env(&r, &folders, T), made.proposals[0].id, "ui"), Err(RepairError::Refused(_))), "a rejected proposal cannot be approved");
    assert!(matches!(approve(&r.store, &env(&r, &folders, T), 9999, "ui"), Err(RepairError::NotFound(_))));
    let dismissed = dismiss_gap(&r.store, gap.id).unwrap();
    assert_eq!(dismissed.status, "dismissed");
    assert_eq!(r.store.proposal(made.proposals[1].id).unwrap().unwrap().status, "rejected");
    assert!(matches!(create_proposals_for(&r.store, &env(&r, &folders, T), gap.id), Err(RepairError::Refused(m)) if m.contains("dismissed")));
    // It happens again: the count grows and the gap stays dismissed.
    let again = platform_gap(&r.store, &flow);
    assert_eq!((again.id, again.count, again.status.as_str()), (gap.id, 2, "dismissed"));
}

#[test]
fn a_folder_the_person_named_is_offered_and_allowing_it_goes_through_the_tool_host_which_asks_again() {
    let r = rig("p_folder");
    let folders = FakeFolders::new(Ok(true), false);
    let gap = folder_gap(&r.store, "E:/work", json!({ "named_by_person": true }));
    let made = create_proposals_for(&r.store, &env(&r, &folders, T), gap.id).unwrap();
    assert_eq!(made.proposals.len(), 1, "{}", made.note);
    let p = &made.proposals[0];
    assert_eq!((p.kind.as_str(), p.payload.clone()), ("allow_folder", json!({ "kind": "allow_folder", "path": "E:/work", "access": "rw" })));
    assert!(p.dry_run.contains("ask you again") && p.dry_run.contains("until Aqua restarts"), "{}", p.dry_run);
    assert!(folders.asked.lock().unwrap().is_empty(), "making the proposal asks the host nothing");
    let done = approve(&r.store, &env(&r, &folders, T + 1), p.id, "ui").unwrap();
    assert_eq!(done.status, "applied", "{done:?}");
    assert_eq!(*folders.asked.lock().unwrap(), vec![("E:/work".to_string(), "rw".to_string())], "the host was asked once, for this folder");
    assert_eq!((done.result["host_decision"].clone(), done.result["request_id"].clone()), (json!("approved"), json!("req_fake")));
    assert_eq!(done.undo["kind"], "revoke_folder");
    assert_eq!(r.store.gap(gap.id).unwrap().unwrap().status, "fixed");
    // The tool host has no way to take a grant back, so the undo says so and the proposal stays applied.
    let refused = undo(&r.store, &env(&r, &folders, T + 2), p.id).unwrap_err();
    assert!(matches!(&refused, RepairError::Refused(m) if m.contains("cannot take a folder back")), "{refused}");
    assert_eq!(r.store.proposal(p.id).unwrap().unwrap().status, "applied");
    // With a host that can take it back, the undo does.
    let r2 = rig("p_folder2");
    let can = FakeFolders::new(Ok(true), true);
    let gap2 = folder_gap(&r2.store, "E:/work", json!({ "named_by_person": true, "access": "ro" }));
    let p2 = create_proposals_for(&r2.store, &env(&r2, &can, T), gap2.id).unwrap().proposals.remove(0);
    assert_eq!(p2.payload["access"], "ro", "a folder that was only read is only asked for reading");
    approve(&r2.store, &env(&r2, &can, T + 1), p2.id, "ui").unwrap();
    let undone = undo(&r2.store, &env(&r2, &can, T + 2), p2.id).unwrap();
    assert_eq!(undone.status, "undone");
    assert_eq!(*can.revoked.lock().unwrap(), vec!["E:/work".to_string()]);
    assert_eq!(r2.store.gap(gap2.id).unwrap().unwrap().status, "new");
}

#[test]
fn a_no_from_the_person_applies_nothing_and_a_folder_the_request_did_not_name_is_never_offered() {
    let r = rig("p_folder_no");
    let no = FakeFolders::new(Err("the tool host did not allow the folder: The user did not approve".into()), false);
    let gap = folder_gap(&r.store, "E:/work", json!({ "named_by_person": true }));
    let p = create_proposals_for(&r.store, &env(&r, &no, T), gap.id).unwrap().proposals.remove(0);
    let out = approve(&r.store, &env(&r, &no, T + 1), p.id, "ui").unwrap();
    assert_eq!(out.status, "failed");
    assert!(out.error.contains("did not approve"), "{}", out.error);
    assert!(out.undo.is_null(), "nothing was applied, so there is nothing to undo");
    assert_eq!(r.store.gap(gap.id).unwrap().unwrap().status, "new");
    // A folder only the model chose, and a place the file guard never hands out: no offer.
    let chosen = folder_gap(&r.store, "E:/chosen_by_the_model", json!({ "named_by_person": false }));
    let none = create_proposals_for(&r.store, &env(&r, &no, T), chosen.id).unwrap();
    assert!(none.proposals.is_empty() && none.note.contains("did not name this folder"), "{}", none.note);
    let guarded = folder_gap(&r.store, "C:/Windows", json!({ "named_by_person": true, "refused_by_guard": true }));
    let none = create_proposals_for(&r.store, &env(&r, &no, T), guarded.id).unwrap();
    assert!(none.proposals.is_empty() && none.note.contains("never hands this kind of place out"), "{}", none.note);
    // The host already had the folder: nothing was added, and the undo says there is nothing to undo.
    let there = FakeFolders::new(Ok(false), false);
    let gap = folder_gap(&r.store, "E:/there", json!({ "named_by_person": true }));
    let p = create_proposals_for(&r.store, &env(&r, &there, T), gap.id).unwrap().proposals.remove(0);
    let done = approve(&r.store, &env(&r, &there, T + 1), p.id, "ui").unwrap();
    assert_eq!(done.undo["kind"], "nothing");
    assert_eq!(undo(&r.store, &env(&r, &there, T + 2), p.id).unwrap().status, "undone");
}

// ---- a request that cannot be met, through the real file tools ---------------------------------------------

struct Answer {
    yes: bool,
}

impl aqua_tool_host::Approver for Answer {
    fn request(&self, _req: aqua_tool_host::ApprovalRequest) -> aqua_tool_host::ProviderFuture<'_, aqua_tool_host::ApprovalOutcome> {
        let out = if self.yes { aqua_tool_host::ApprovalOutcome::Approved(aqua_tool_host::ApprovalScope::Once) } else { aqua_tool_host::ApprovalOutcome::Denied };
        Box::pin(async move { out })
    }
}

/// A host with the real file tools, one trusted folder, and a person who answers `yes` or no to every question. (The temporary folder of a test is inside AppData, which the
/// folder rules keep apart, so the guard is told to keep nothing apart.)
fn fs_host(name: &str, yes: bool) -> (aqua_tool_host::ToolHost, std::path::PathBuf) {
    use aqua_native_tools::fs::{Access, FsConfig, FsProvider, PathGuard, Root, Trash};
    // The process-wide store must be installed before the file tools can write a gap into it.
    global();
    let dir = temp_dir(name);
    for d in ["ws", "outside", "aqua/tools"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let cfg = FsConfig { roots: vec![Root { alias: "ws".into(), path: dir.join("ws").to_string_lossy().into(), access: Access::Rw, trusted: true }], ..FsConfig::with_workspace("unused") };
    let guard = Arc::new(PathGuard::new(cfg, vec![dir.join("aqua")]).unwrap());
    guard.set_kept(vec![]);
    let trash = Arc::new(Trash::new(dir.join("aqua").join("trash")).unwrap());
    let host = aqua_tool_host::ToolHost::new(Arc::new(aqua_tool_host::PolicyEngine::new(aqua_tool_host::Mode::Standard)), Arc::new(aqua_tool_host::AuditLedger::open(dir.join("aqua/tools/audit.jsonl"))));
    host.set_approver(Arc::new(Answer { yes }));
    host.register(Arc::new(FsProvider::new(guard, trash))).unwrap();
    (host, dir)
}

fn folder_gaps_of_run(run: &str) -> Vec<Gap> {
    global().gaps(None, 500).unwrap().into_iter().filter(|g| g.kind == "folder_not_allowed" && g.run_id == run).collect()
}

#[test]
fn a_folder_the_request_named_and_the_person_refused_is_one_gap_that_counts_and_can_be_proposed() {
    let (h, dir) = fs_host("sr_no", false);
    let outside = dir.join("outside");
    let run = format!("sr_run_no_{}", unique());
    crate::sources::begin(&run, &format!("write a hello world in {}", outside.display()));
    let file = format!("{}\\hello.py", outside.display());
    let said = crate::agent_tools::call(&h, "fs.write", json!({"path": file, "content": "x"}), aqua_tool_host::Caller::Agent, &run);
    assert!(said.starts_with("ERROR [CONFIRMATION_DENIED] fs.allow_folder"), "{said}");
    let second = crate::agent_tools::call(&h, "fs.write", json!({"path": file, "content": "x"}), aqua_tool_host::Caller::Agent, &run);
    assert!(second.starts_with("ERROR [INVALID_ARGUMENTS] fs.write:"), "the person is not asked twice: {second}");
    crate::sources::end(&run);
    // One gap for the folder: the person's no (asked once) and the refusal that stood after it (the second call) are the same gap.
    let gaps = folder_gaps_of_run(&run);
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    let gap = &gaps[0];
    assert_eq!(gap.count, 2);
    assert_eq!(gap.key, super::gaps::folder_key(&outside.display().to_string()));
    assert_eq!((gap.data["named_by_person"].clone(), gap.data["refused_by_guard"].clone(), gap.data["access"].clone()), (json!(true), json!(false), json!("rw")), "{}", gap.data);
    assert!(gap.summary.contains("the request named it") && gap.asked.starts_with("write a hello world in "), "{} / {}", gap.summary, gap.asked);
    assert_eq!(gap.source, "agent_run");
    // The gap can be turned into an offer to allow that folder, and making the offer does not ask the host anything.
    let r = rig("sr_no_offer");
    let moved = r.store.record(NewGap { data: gap.data.clone(), ..NewGap::new(GapKind::FolderNotAllowed, gap.key.clone(), gap.summary.clone()) }, T).unwrap();
    let folders = FakeFolders::new(Ok(true), false);
    let made = create_proposals_for(&r.store, &env(&r, &folders, T), moved.id).unwrap();
    assert_eq!(made.proposals.len(), 1, "{}", made.note);
    assert_eq!(made.proposals[0].kind, "allow_folder");
    assert!(folders.asked.lock().unwrap().is_empty());
}

#[test]
fn a_folder_the_person_said_yes_to_is_not_a_gap_and_a_request_that_names_no_folder_is_a_gap_that_is_never_offered() {
    // Yes: the call goes through, so nothing could not be done.
    let (h, dir) = fs_host("sr_yes", true);
    let outside = dir.join("outside");
    let run = format!("sr_run_yes_{}", unique());
    crate::sources::begin(&run, &format!("can u write a hello world in \"{}\\\" folder for me", outside.display()));
    let said = crate::agent_tools::call(&h, "fs.write", json!({"path": format!("{}\\hello.py", outside.display()), "content": "x"}), aqua_tool_host::Caller::Agent, &run);
    assert!(!said.starts_with("ERROR"), "{said}");
    crate::sources::end(&run);
    assert!(folder_gaps_of_run(&run).is_empty());
    // No folder named: the call is refused, nobody is asked, and the gap says the folder was not named.
    let run = format!("sr_run_none_{}", unique());
    crate::sources::begin(&run, "write a hello world in python");
    let said = crate::agent_tools::call(&h, "fs.write", json!({"path": format!("{}\\elsewhere.py", dir.join("unnamed").display()), "content": "x"}), aqua_tool_host::Caller::Agent, &run);
    assert!(said.starts_with("ERROR [INVALID_ARGUMENTS] fs.write:"), "{said}");
    crate::sources::end(&run);
    let gaps = folder_gaps_of_run(&run);
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!((gaps[0].data["named_by_person"].clone(), gaps[0].count), (json!(false), 1));
    assert!(gaps[0].summary.starts_with("A file tool was refused the folder"), "{}", gaps[0].summary);
    assert_eq!(gaps[0].key, super::gaps::folder_key(&dir.join("unnamed").display().to_string()), "the folder that holds the refused file");
    let r = rig("sr_none_offer");
    let moved = r.store.record(NewGap { data: gaps[0].data.clone(), ..NewGap::new(GapKind::FolderNotAllowed, gaps[0].key.clone(), gaps[0].summary.clone()) }, T).unwrap();
    let folders = FakeFolders::new(Ok(true), false);
    let made = create_proposals_for(&r.store, &env(&r, &folders, T), moved.id).unwrap();
    assert!(made.proposals.is_empty() && made.note.contains("did not name this folder"), "{}", made.note);
}

// ---- the file (OA3-02: the shared SQLite helper) -----------------------------------------------------------------

#[test]
fn a_gap_database_from_a_newer_build_is_not_opened_for_writing_and_not_changed() {
    let path = temp_dir("store_newer").join("self_repair.db");
    {
        // A newer build's file has the same tables, so a build that went on regardless would read and write it without any error: the version is what stops it.
        let store = RepairStore::new(&path);
        store.record(NewGap::new(GapKind::Other, "k", "from a newer build"), T).unwrap();
        let c = rusqlite::Connection::open(&path).unwrap();
        let _: String = c.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0)).unwrap();
        c.execute_batch("PRAGMA user_version = 77;").unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let store = RepairStore::new(&path);
    let err = store.gaps(None, 10).unwrap_err();
    assert!(err.to_string().contains("newer than this build"), "{err}");
    let err = store.record(NewGap::new(GapKind::Other, "k2", "s"), T).unwrap_err();
    assert!(err.to_string().contains("version 77"), "{err}");
    drop(store);
    assert_eq!(std::fs::read(&path).unwrap(), before, "byte for byte what it was");
}

#[test]
fn a_damaged_gap_database_is_moved_aside_and_a_new_one_takes_its_place() {
    let dir = temp_dir("store_damaged");
    let path = dir.join("self_repair.db");
    std::fs::write(&path, "this is not a database at all, ".repeat(200)).unwrap();
    let store = RepairStore::new(&path);
    let gap = store.record(NewGap::new(GapKind::Other, "k", "after the recovery"), T).unwrap();
    assert_eq!(store.gaps(None, 10).unwrap().len(), 1);
    assert_eq!(store.gap(gap.id).unwrap().unwrap().summary, "after the recovery");
    let aside = crate::storage::sqlite_store::leftovers(&dir);
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(std::fs::read_to_string(&aside[0]).unwrap().starts_with("this is not a database"), "the damaged file is kept");
}
