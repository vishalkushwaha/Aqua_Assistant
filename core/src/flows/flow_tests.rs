use super::diff::diff;
use super::model::*;
use super::registry;
use super::store::*;
use super::templates;
use super::validate::*;
use serde_json::json;

fn store(name: &str) -> FlowStore {
    let dir = std::env::temp_dir().join(format!("aqua_flows_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    FlowStore::new(dir.join("flows.db"))
}

fn social() -> FlowDef {
    let mut d = templates::social_post();
    d.normalize();
    d
}

fn chain() -> FlowDef {
    let mut d = FlowDef {
        name: "Small".into(),
        nodes: vec![
            Node::new("start", NodeKind::Trigger, "Start").with_config(json!({ "trigger_type": "manual" })),
            Node::new("write", NodeKind::TextGeneration, "Write").with_config(json!({ "prompt": "Say hi" })),
            Node::new("save", NodeKind::File, "Save").with_config(json!({ "operation": "write", "path": "hi.txt", "content": "{{nodes.write.text}}" })),
        ],
        edges: vec![Edge::new("start", "write", "out"), Edge::new("write", "save", "out")],
        ..Default::default()
    };
    d.normalize();
    d
}

fn codes(d: &FlowDef) -> Vec<String> {
    validate(d).issues.iter().map(|i| i.code.clone()).collect()
}

// ---- registry and model --------------------------------------------------------------------------

#[test]
fn every_node_kind_has_a_registry_entry_with_sane_ports_and_fields() {
    for k in NodeKind::ALL {
        let s = registry::spec(*k);
        assert_eq!(s.kind, *k);
        assert!(!s.label.is_empty() && !s.description.is_empty() && !s.out_ports.is_empty(), "{k:?}");
        assert_eq!(serde_json::to_value(k).unwrap(), json!(k.as_str()), "the name on the wire is the registry name");
        for f in &s.fields {
            if f.kind == registry::FieldKind::Choice {
                assert!(!f.choices.is_empty(), "{k:?}.{}", f.name);
                if let Some(d) = &f.default {
                    assert!(f.choices.contains(&d.as_str().unwrap().to_string()), "{k:?}.{} default is a choice", f.name);
                }
            }
        }
    }
    assert_eq!(registry::all().len(), NodeKind::ALL.len());
    assert!(registry::spec(NodeKind::SocialMedia).risk == registry::Risk::High);
    assert_eq!(registry::spec(NodeKind::Condition).out_ports, ["true", "false"]);
    assert_eq!(registry::spec(NodeKind::Approval).out_ports, ["approved", "rejected"]);
    assert!(registry::kinds_with("web_search").contains(&NodeKind::Research));
    assert!(registry::platforms().contains(&"instagram") && registry::platform_actions().contains(&"schedule"));
}

#[test]
fn a_flow_round_trips_through_json_and_normalize_fills_in_what_was_left_out() {
    let raw = json!({
        "name": "  Demo ",
        "nodes": [{ "id": "start", "kind": "trigger" }, { "id": "w", "kind": "text_generation", "config": { "prompt": "hi" } }],
        "edges": [{ "from": "start", "to": "w" }]
    });
    let mut d: FlowDef = serde_json::from_value(raw).unwrap();
    d.normalize();
    assert_eq!(d.name, "Demo");
    assert_eq!(d.nodes[0].name, "Trigger", "a missing name becomes the kind's label");
    assert_eq!(d.nodes[1].outputs, ["text"], "default outputs are filled in");
    assert_eq!((d.edges[0].port.as_str(), d.edges[0].id.as_str()), ("out", "start:out->w"));
    assert!(d.nodes[0].enabled && d.nodes[0].retry.retries == 0 && d.nodes[0].on_error == OnError::Fail);
    let back: FlowDef = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
    assert_eq!(back, d);
    assert!(serde_json::from_value::<FlowDef>(json!({ "name": "x", "nodes": [{ "id": "a", "kind": "no_such_kind" }] })).is_err());
    assert_eq!(d.successors("start", None), ["w"]);
    assert_eq!(d.predecessors("w"), ["start"]);
}

#[test]
fn size_limits_are_enforced() {
    let mut d = chain();
    d.nodes = (0..101).map(|i| Node::new(&format!("n{i}"), NodeKind::Delay, "d")).collect();
    assert!(d.check_size().unwrap_err().contains("at most 100"));
    let mut big = chain();
    big.description = "x".repeat(600_000);
    assert!(big.check_size().unwrap_err().contains("too large"));
}

// ---- validation ------------------------------------------------------------------------------------

#[test]
fn the_social_post_template_is_a_valid_flow_that_will_ask_before_nothing_because_it_has_an_approval() {
    let d = social();
    let r = validate(&d);
    assert!(r.ok(), "{:#?}", r.issues);
    assert!(r.needs_approval.is_empty(), "the Approve step covers publishing");
    assert_eq!(d.nodes.len(), 14);
    let json = serde_json::to_string(&d).unwrap();
    assert!(!json.contains("password") && json.contains("linkedin/main"), "a credential is named, never held");
    // Removing the approval step makes publishing ask by itself, and shows why.
    let mut no_gate = d.clone();
    no_gate.nodes.retain(|n| n.id != "preview");
    no_gate.edges.retain(|e| e.from != "preview" && e.to != "preview");
    no_gate.edges.push(Edge::new("logged_in", "publish", "true"));
    no_gate.edges.push(Edge::new("ask_login", "publish", "out"));
    let r = validate(&no_gate);
    assert!(r.ok(), "{:#?}", r.issues);
    assert_eq!(r.needs_approval, ["publish"]);
    assert!(r.has("will_ask"));
    // With approvals off for the flow it is allowed, with a warning, never silently.
    let mut never = no_gate.clone();
    never.settings.approval = ApprovalPolicy::Never;
    let r = validate(&never);
    assert!(r.ok() && r.needs_approval.is_empty() && r.has("no_approval"));
}

#[test]
fn structure_problems_are_found_and_pinned_to_a_node() {
    // duplicate id, dangling edge, bad port, self loop, edge into a trigger
    let mut d = chain();
    d.nodes.push(Node::new("write", NodeKind::Delay, "Dup").with_config(json!({ "seconds": 5 })));
    d.edges.push(Edge::new("write", "ghost", "out"));
    d.edges.push(Edge::new("write", "save", "nope"));
    d.edges.push(Edge::new("save", "save", "out"));
    d.edges.push(Edge::new("save", "start", "out"));
    let c = codes(&d);
    for want in ["duplicate_id", "dangling_edge", "bad_port", "self_loop", "trigger_has_input"] {
        assert!(c.contains(&want.to_string()), "{want} in {c:?}");
    }
    assert!(!validate(&d).ok());
    // cycle
    let mut d = chain();
    d.nodes.push(Node::new("again", NodeKind::Delay, "Again").with_config(json!({ "seconds": 1 })));
    d.edges = vec![Edge::new("start", "write", "out"), Edge::new("write", "save", "out"), Edge::new("save", "again", "out"), Edge::new("again", "write", "out")];
    let r = validate(&d);
    assert!(r.has("cycle"), "{:?}", r.issues);
    assert!(r.issues.iter().find(|i| i.code == "cycle").unwrap().message.contains("For each"));
    // no trigger, empty flow, no name
    let mut d = chain();
    d.nodes.remove(0);
    d.edges.remove(0);
    assert!(codes(&d).contains(&"no_trigger".into()));
    assert!(codes(&FlowDef { name: "x".into(), ..Default::default() }).contains(&"no_nodes".into()));
    let mut d = chain();
    d.name = " ".into();
    assert!(codes(&d).contains(&"no_name".into()));
    // unreachable, and because an earlier step is switched off
    let mut d = chain();
    d.nodes.push(Node::new("island", NodeKind::Delay, "Island").with_config(json!({ "seconds": 5 })));
    assert!(validate(&d).for_node("island").iter().any(|i| i.code == "unreachable"));
    // A switched-off step is skipped; the flow carries on past it.
    let mut d = chain();
    d.node_mut("write").unwrap().enabled = false;
    d.nodes[2].config = json!({ "operation": "write", "path": "hi.txt", "content": "static" });
    let r = validate(&d);
    assert!(r.ok() && r.for_node("save").iter().all(|i| i.code != "unreachable"), "{:?}", r.issues);
    // bad id
    let mut d = chain();
    d.nodes[1].id = "Has Space".into();
    assert!(codes(&d).contains(&"bad_id".into()));
}

#[test]
fn configuration_is_checked_against_the_registry() {
    let with = |kind: NodeKind, cfg: serde_json::Value| {
        let mut d = chain();
        d.nodes[1] = Node::new("write", kind, "Step").with_config(cfg);
        d.nodes[1].outputs = vec!["text".into()];
        validate(&d)
    };
    assert!(with(NodeKind::TextGeneration, json!({})).has("missing_config"), "prompt is required");
    assert!(with(NodeKind::TextGeneration, json!({ "prompt": "x", "max_words": "many" })).has("bad_config"));
    assert!(with(NodeKind::TextGeneration, json!({ "prompt": "x", "tone": "angry" })).has("bad_config"), "not one of the choices");
    assert!(with(NodeKind::TextGeneration, json!({ "prompt": "x", "tone": "{{vars.tone}}" })).issues.iter().all(|i| i.code != "bad_config"), "a reference is checked elsewhere");
    assert!(with(NodeKind::Website, json!({ "url": "linkedin.com" })).has("bad_config"));
    assert!(with(NodeKind::Http, json!({ "method": "GET", "url": "https://x.example", "headers": "{not json" })).has("bad_config"));
    assert!(with(NodeKind::Http, json!({ "method": "GET", "url": "https://x.example", "headers": "{\"a\":\"b\"}" })).issues.iter().all(|i| i.code != "bad_config"));
    assert!(with(NodeKind::Delay, json!({ "seconds": 5, "colour": "red" })).has("unknown_config"));
    assert!(with(NodeKind::Trigger, json!({ "trigger_type": "schedule", "cron": "99 * * * *" })).has("bad_schedule"));
    assert!(with(NodeKind::Trigger, json!({ "trigger_type": "schedule" })).has("missing_config"));
    assert!(with(NodeKind::Trigger, json!({ "trigger_type": "schedule", "cron": "0 9 * * *" })).issues.iter().all(|i| i.code != "bad_schedule" && i.code != "missing_config"));
}

#[test]
fn secrets_are_refused_in_a_flow_and_credentials_are_only_names() {
    let mut d = chain();
    d.nodes[1] = Node::new("write", NodeKind::Http, "Call").with_config(json!({ "method": "GET", "url": "https://x.example", "headers": { "Authorization": "Bearer abc123" } }));
    let r = validate(&d);
    assert!(r.has("secret_in_config") && r.issues.iter().any(|i| i.message.contains("credential")), "{:?}", r.issues);
    let mut d = chain();
    d.nodes[1].credential = Some("linkedin/main".into());
    assert!(!codes(&d).contains(&"bad_credential".into()));
    d.nodes[1].credential = Some("../etc/passwd".into());
    assert!(codes(&d).contains(&"bad_credential".into()));
    let mut d = chain();
    d.nodes[2].config = json!({ "operation": "write", "path": "x.txt", "content": "key sk-live-abcdef123456" });
    assert!(codes(&d).contains(&"secret_in_config".into()));
    // An email address is not a secret: a step that fills in a form may type one. A key next to it still is.
    d.nodes[2].config = json!({ "operation": "write", "path": "x.txt", "content": "write to ada@example.com about the trip" });
    assert!(!codes(&d).contains(&"secret_in_config".into()));
    d.nodes[2].config = json!({ "operation": "write", "path": "x.txt", "content": "ada@example.com key sk-live-abcdef123456" });
    assert!(codes(&d).contains(&"secret_in_config".into()));
}

#[test]
fn references_must_point_upstream_at_outputs_that_exist() {
    let mut d = chain();
    d.nodes[2].config = json!({ "operation": "write", "path": "x.txt", "content": "{{nodes.write.nothing}}" });
    assert!(codes(&d).contains(&"missing_output".into()));
    d.nodes[2].config = json!({ "operation": "write", "path": "x.txt", "content": "{{nodes.ghost.text}}" });
    assert!(codes(&d).contains(&"missing_node_ref".into()));
    d.nodes[2].config = json!({ "operation": "write", "path": "x.txt", "content": "{{nodes.write}}" });
    assert!(codes(&d).contains(&"bad_reference".into()));
    d.nodes[2].config = json!({ "operation": "write", "path": "x.txt", "content": "{{bogus.thing}}" });
    assert!(codes(&d).contains(&"unknown_reference".into()));
    // A step cannot use what comes after it, or itself.
    let mut d = chain();
    d.nodes[1].config = json!({ "prompt": "{{nodes.save.file}}" });
    assert!(codes(&d).contains(&"not_upstream".into()));
    d.nodes[1].config = json!({ "prompt": "{{nodes.write.text}}" });
    assert!(codes(&d).contains(&"not_upstream".into()));
    // Variables: from the flow's inputs or a Set a value step; with a fallback filter it still counts.
    let mut d = chain();
    d.nodes[1].config = json!({ "prompt": "About {{vars.topic | default: AI}}" });
    assert!(codes(&d).contains(&"undefined_variable".into()));
    d.variables.push(VariableDef { name: "topic".into(), label: "Topic".into(), kind: VarKind::Text, required: true, default: None, choices: vec![] });
    assert!(!codes(&d).contains(&"undefined_variable".into()));
    // Trigger, now, flow and run values are always available.
    d.nodes[1].config = json!({ "prompt": "{{trigger.payload.x}} {{now.date}} {{flow.name}} {{run.id}}" });
    assert!(validate(&d).ok());
    // A disabled step's output is a warning, not an error.
    let mut d = chain();
    d.nodes[1].enabled = false;
    let r = validate(&d);
    assert!(r.has("disabled_ref") && r.ok(), "{:?}", r.issues);
}

#[test]
fn open_branches_and_variable_definitions_are_reported() {
    let mut d = chain();
    d.nodes.push(Node::new("check", NodeKind::Condition, "Check").with_config(json!({ "expression": "{{vars.x}}" })));
    d.variables.push(VariableDef { name: "x".into(), label: "".into(), kind: VarKind::Text, required: false, default: None, choices: vec![] });
    d.edges.push(Edge::new("save", "check", "out"));
    d.edges.push(Edge::new("check", "write", "true"));
    let r = validate(&d);
    assert!(r.has("cycle"), "true -> write goes back");
    let mut d = chain();
    d.nodes.push(Node::new("check", NodeKind::Condition, "Check").with_config(json!({ "expression": "{{vars.x}}" })));
    d.variables.push(VariableDef { name: "x".into(), label: "".into(), kind: VarKind::Text, required: false, default: None, choices: vec![] });
    d.edges.push(Edge::new("save", "check", "out"));
    let r = validate(&d);
    assert_eq!(r.issues.iter().filter(|i| i.code == "open_branch").count(), 2, "true and false are both open");
    assert!(r.ok(), "open branches are warnings");
    d.variables.push(VariableDef { name: "x".into(), label: "".into(), kind: VarKind::Text, required: false, default: None, choices: vec![] });
    d.variables.push(VariableDef { name: "pick".into(), label: "".into(), kind: VarKind::Choice, required: false, default: None, choices: vec![] });
    let c = codes(&d);
    assert!(c.contains(&"duplicate_variable".into()) && c.contains(&"bad_variable".into()));
}

#[test]
fn a_condition_that_cannot_be_read_is_an_error_when_the_flow_is_saved_and_a_bad_template_a_warning() {
    let with_condition = |expression: &str| {
        let mut d = chain();
        for name in ["x", "y"] {
            d.variables.push(VariableDef { name: name.into(), label: "".into(), kind: VarKind::Text, required: false, default: None, choices: vec![] });
        }
        d.nodes.push(Node::new("check", NodeKind::Condition, "Check").with_config(json!({ "expression": expression })));
        d.edges.push(Edge::new("save", "check", "out"));
        validate(&d)
    };
    for good in ["{{vars.x}}", "{{vars.x}} == 3 && !({{vars.y | length}} > 1)", "true", "{{vars.x}} contains \"a b\""] {
        assert!(!with_condition(good).has("bad_condition"), "{good}");
    }
    for bad in ["{{vars.x}} ==", "(1 == 1", "{{vars.x | shout}} == 1", "{{vars.x"] {
        let r = with_condition(bad);
        assert!(r.has("bad_condition") && !r.ok(), "{bad}: {:?}", r.issues);
    }
    let mut d = chain();
    d.nodes[1].config = json!({ "prompt": "Summarise {{trigger.payload.x | shout}}" });
    let r = validate(&d);
    assert!(r.has("bad_template") && r.ok(), "{:?}", r.issues);
}

// ---- diff ------------------------------------------------------------------------------------------

#[test]
fn a_diff_says_what_was_added_removed_changed_and_moved() {
    let a = social();
    let mut b = a.clone();
    // Instagram instead of LinkedIn
    b.node_mut("publish").unwrap().config["platform"] = json!("instagram");
    b.node_mut("publish").unwrap().name = "Publish to Instagram".into();
    b.node_mut("open_site").unwrap().config["url"] = json!("https://www.instagram.com");
    b.node_mut("start").unwrap().position = Position { x: 500.0, y: 40.0 };
    let d = diff(&a, &b);
    assert!(d.nodes_added.is_empty() && d.nodes_removed.is_empty() && d.edges_added.is_empty());
    assert_eq!(d.nodes_moved, ["start"]);
    let publish = d.nodes_changed.iter().find(|c| c.id == "publish").unwrap();
    assert_eq!(publish.fields, ["name", "config.platform"]);
    assert!(d.summary.iter().any(|s| s == "Changed: Publish to Instagram (name, config.platform)"), "{:?}", d.summary);
    // only moving nodes is not an edit
    let mut m = a.clone();
    m.node_mut("research").unwrap().position.x += 40.0;
    assert!(diff(&a, &m).only_moves() && diff(&a, &m).is_empty());
    // add and remove
    let mut c = a.clone();
    c.nodes.retain(|n| n.id != "make_image");
    c.edges.retain(|e| e.from != "make_image" && e.to != "make_image");
    c.edges.push(Edge::new("image_prompt", "open_site", "out"));
    c.nodes.push(Node::new("tw", NodeKind::SocialMedia, "Post to X"));
    let d = diff(&a, &c);
    assert_eq!((d.nodes_removed.as_slice(), d.nodes_added.as_slice()), (&["make_image".to_string()][..], &["tw".to_string()][..]));
    assert!(d.summary.contains(&"Removed: Generate the image".to_string()) && d.summary.contains(&"Added: Post to X".to_string()));
    assert!(d.summary.iter().any(|s| s == "Connected: Write the image prompt -> Open the platform"));
    assert!(d.summary.iter().any(|s| s == "Disconnected: Generate the image -> Open the platform"));
    assert!(diff(&a, &a).is_empty());
    let mut r = a.clone();
    r.name = "Other".into();
    r.settings.approval = ApprovalPolicy::Never;
    assert_eq!(diff(&a, &r).flow_changed, ["name", "settings"]);
}

// ---- store -----------------------------------------------------------------------------------------

#[test]
fn saving_creates_versions_but_not_for_no_ops_or_node_moves() {
    let s = store("versions");
    let f = s.create("local", chain(), "user", "first draft", 100).unwrap();
    assert_eq!((f.version, f.def.name.as_str(), f.owner_id.as_str()), (1, "Small", "local"));
    // no change: same version
    assert_eq!(s.update("local", f.id, chain(), "user", "", Some(1), 110).unwrap().unwrap().version, 1);
    // moving nodes updates in place, no new version
    let mut moved = chain();
    moved.nodes[1].position = Position { x: 300.0, y: 300.0 };
    let m = s.update("local", f.id, moved, "user", "", Some(1), 120).unwrap().unwrap();
    assert_eq!((m.version, m.def.nodes[1].position.x), (1, 300.0));
    assert_eq!(s.versions("local", f.id).unwrap().len(), 1);
    // a real change is version 2, with author and note
    let mut edited = m.def.clone();
    edited.nodes[1].config["prompt"] = json!("Say hello");
    let v2 = s.update("local", f.id, edited, "ai", "made it friendlier", Some(1), 130).unwrap().unwrap();
    assert_eq!((v2.version, v2.updated_at), (2, 130));
    let versions = s.versions("local", f.id).unwrap();
    assert_eq!(versions.iter().map(|v| (v.version, v.author.as_str(), v.note.as_str())).collect::<Vec<_>>(), [(2, "ai", "made it friendlier"), (1, "user", "first draft")]);
    // a stale editor is refused
    let mut stale = chain();
    stale.nodes[1].config["prompt"] = json!("stale");
    let e = s.update("local", f.id, stale, "user", "", Some(1), 140).unwrap_err();
    assert!(e.contains("conflict") && e.contains("version 2"), "{e}");
    assert_eq!(s.get("local", f.id).unwrap().unwrap().def.nodes[1].config["prompt"], "Say hello");
    // an invalid draft can be saved (it is validated when it runs), but not a nameless or huge one
    let mut draft = s.get("local", f.id).unwrap().unwrap().def;
    draft.edges.clear();
    assert!(s.update("local", f.id, draft.clone(), "user", "draft", None, 150).is_ok());
    draft.name = "".into();
    assert!(s.update("local", f.id, draft, "user", "", None, 160).is_err());
}

#[test]
fn versions_can_be_read_compared_and_restored_and_a_restore_can_be_undone() {
    let s = store("restore");
    let f = s.create("local", chain(), "user", "", 100).unwrap();
    let mut e = chain();
    e.nodes[1].config["prompt"] = json!("v2 prompt");
    s.update("local", f.id, e, "user", "", None, 110).unwrap();
    let old = s.version("local", f.id, 1).unwrap().unwrap();
    assert_eq!(old.nodes[1].config["prompt"], "Say hi");
    assert!(s.version("local", f.id, 9).unwrap().is_none());
    let restored = s.restore("local", f.id, 1, "user", 120).unwrap().unwrap();
    assert_eq!((restored.version, restored.def.nodes[1].config["prompt"].clone()), (3, json!("Say hi")));
    assert_eq!(s.versions("local", f.id).unwrap()[0].note, "restored version 1");
    // undo the restore by restoring version 2
    let undone = s.restore("local", f.id, 2, "user", 130).unwrap().unwrap();
    assert_eq!((undone.version, undone.def.nodes[1].config["prompt"].clone()), (4, json!("v2 prompt")));
    assert!(s.restore("local", f.id, 99, "user", 140).unwrap().is_none());
}

#[test]
fn history_is_bounded_and_the_audit_trail_records_who_did_what() {
    let s = store("audit");
    let f = s.create("local", chain(), "user", "", 100).unwrap();
    for i in 0..(KEEP_VERSIONS + 5) {
        let mut d = chain();
        d.nodes[1].config["prompt"] = json!(format!("p{i}"));
        s.update("local", f.id, d, if i % 2 == 0 { "ai" } else { "user" }, "", None, 200 + i).unwrap();
    }
    let v = s.versions("local", f.id).unwrap();
    assert_eq!(v.len() as i64, KEEP_VERSIONS);
    assert_eq!(v[0].version, KEEP_VERSIONS + 6, "the current version is kept");
    let log = s.audit_log("local", Some(f.id), 500).unwrap();
    assert!(log.iter().any(|e| e.action == "create" && e.actor == "user") && log.iter().any(|e| e.action == "update" && e.actor == "ai"));
    assert!(log.iter().any(|e| e.detail.contains("Changed: Write (config.prompt)")), "{:?}", log[0]);
    s.audit("local", f.id, "user", "run_started", "run 1", 999).unwrap();
    assert_eq!(s.audit_log("local", Some(f.id), 1).unwrap()[0].action, "run_started");
    assert!(s.delete("local", f.id, "user", 1000).unwrap());
    assert!(s.audit_log("local", None, 1).unwrap()[0].action == "delete", "deleting is recorded even though the flow is gone");
    assert!(s.get("local", f.id).unwrap().is_none() && s.versions("local", f.id).unwrap().is_empty());
}

#[test]
fn one_owner_never_sees_or_changes_anothers_flows() {
    let s = store("owners");
    let mine = s.create("alice", chain(), "user", "", 100).unwrap();
    let theirs = s.create("bob", chain(), "user", "", 100).unwrap();
    assert_eq!(s.list("alice").unwrap().len(), 1);
    assert_eq!(s.list("alice").unwrap()[0].id, mine.id);
    assert!(s.get("alice", theirs.id).unwrap().is_none());
    let mut d = chain();
    d.name = "Hijack".into();
    assert!(s.update("alice", theirs.id, d, "user", "", None, 110).unwrap().is_none());
    assert!(s.versions("alice", theirs.id).unwrap().is_empty() && s.version("alice", theirs.id, 1).unwrap().is_none());
    assert!(s.restore("alice", theirs.id, 1, "user", 120).unwrap().is_none());
    assert!(!s.delete("alice", theirs.id, "user", 130).unwrap());
    assert!(s.audit_log("alice", Some(theirs.id), 10).unwrap().is_empty());
    assert_eq!(s.get("bob", theirs.id).unwrap().unwrap().def.name, "Small");
}

#[test]
fn the_list_summarises_and_templates_can_be_built() {
    let s = store("list");
    s.create("local", chain(), "user", "", 100).unwrap();
    let f = s.create("local", social(), "user", "", 200).unwrap();
    let l = s.list("local").unwrap();
    assert_eq!((l[0].id, l[0].name.as_str(), l[0].node_count, l[0].version), (f.id, "AI social post", 14, 1), "newest first");
    for t in templates::list() {
        let d = templates::build(t.id).unwrap();
        assert!(validate(&d).ok(), "{}: {:?}", t.id, validate(&d).issues);
    }
    assert!(templates::build("nope").is_none());
}

#[test]
fn a_condition_on_a_number_or_a_true_false_value_is_read_by_what_it_is_not_as_empty_text() {
    use super::run::manager::FlowRunner;
    use super::run::store::RunStore;
    let dir = std::env::temp_dir().join(format!("aqua_condition_types_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("flows.db");
    let (flows, runs) = (std::sync::Arc::new(FlowStore::new(&path)), std::sync::Arc::new(RunStore::new(&path)));
    let runner = FlowRunner::with_clock(flows.clone(), runs.clone(), std::sync::Arc::new(super::run::exec::Executors::new()), std::sync::Arc::new(|_, c| !c.load(std::sync::atomic::Ordering::SeqCst)), std::sync::Arc::new(|| 1_800_000_000));
    let def: FlowDef = serde_json::from_value(serde_json::json!({
        "name": "Types",
        "nodes": [
            { "id": "start", "kind": "trigger" },
            { "id": "yes", "kind": "condition", "config": { "expression": "{{trigger.payload.yes}}" } },
            { "id": "no", "kind": "condition", "config": { "expression": "{{trigger.payload.no}}" } },
            { "id": "zero", "kind": "condition", "config": { "expression": "{{trigger.payload.zero}}" } },
            { "id": "two", "kind": "condition", "config": { "expression": "{{trigger.payload.two}}" } }
        ],
        "edges": [{ "from": "start", "to": "yes" }, { "from": "start", "to": "no" }, { "from": "start", "to": "zero" }, { "from": "start", "to": "two" }]
    })).unwrap();
    let id = flows.create("local", def, "user", "", 1_800_000_000).unwrap().id;
    let run = runner.start("local", id, Default::default(), serde_json::json!({ "yes": true, "no": false, "zero": 0, "two": 2 }), false, "manual").unwrap();
    assert!(runner.wait_idle(std::time::Duration::from_secs(15)));
    let done = runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(done.status, super::run::model::RunStatus::Succeeded, "{}", done.error);
    let result = |n: &str| runs.node_run(run.id, n).unwrap().unwrap().outputs["result"].clone();
    assert_eq!((result("yes"), result("no"), result("zero"), result("two")), (true.into(), false.into(), false.into(), true.into()));
}

// ---- the file shared by the five flow stores (OA3-02: the shared SQLite helper) ------------------------------

fn flows_file(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aqua_flows_file_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("flows.db")
}

#[test]
fn flows_db_has_a_version_and_one_made_before_versions_is_stamped_and_keeps_its_flows() {
    let path = flows_file("version");
    let s = FlowStore::new(&path);
    let made = s.create("local", chain(), "tester", "first", 1_800_000_000).unwrap();
    let version = || -> i64 { rusqlite::Connection::open(&path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap() };
    assert_eq!(version(), 1);
    rusqlite::Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 0;").unwrap();
    let again = FlowStore::new(&path);
    assert_eq!(version(), 1);
    assert_eq!(again.list("local").unwrap().iter().map(|f| f.id).collect::<Vec<_>>(), vec![made.id]);
}

#[test]
fn a_damaged_flows_db_is_moved_aside_by_the_flow_store_and_every_store_of_the_file_works_afterwards() {
    let path = flows_file("damaged");
    std::fs::write(&path, "this is not a database, only text. ".repeat(300)).unwrap();
    let flows = FlowStore::new(&path);
    let made = flows.create("local", chain(), "tester", "first", 1_800_000_000).unwrap();
    assert_eq!(flows.list("local").unwrap().len(), 1);
    // The run store of the same file finds its tables, made by the flow store's start-up.
    let runs = super::run::store::RunStore::new(&path);
    assert!(runs.list_runs("local", Some(made.id), 10).unwrap().is_empty());
    let aside = crate::storage::sqlite_store::leftovers(path.parent().unwrap());
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(std::fs::read_to_string(&aside[0]).unwrap().starts_with("this is not a database"), "the old file is kept");
}

#[test]
fn a_flows_db_from_a_newer_build_is_refused_by_all_five_stores_and_left_alone() {
    let path = flows_file("newer");
    let flows = FlowStore::new(&path);
    flows.create("local", chain(), "tester", "first", 1_800_000_000).unwrap();
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        let _: String = c.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0)).unwrap();
        c.execute_batch("PRAGMA user_version = 4;").unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let flows = FlowStore::new(&path);
    let err = flows.list("local").unwrap_err();
    assert!(err.contains("newer than this build") && err.contains("version 4"), "{err}");
    assert!(flows.create("local", chain(), "tester", "must not be written", 1_800_000_001).is_err());
    let runs = super::run::store::RunStore::new(&path);
    assert!(runs.list_runs("local", None, 10).unwrap_err().contains("newer than this build"));
    let scheds = super::sched::store::SchedStore::new(&path);
    assert!(scheds.list("local", None).unwrap_err().contains("newer than this build"));
    let notes = super::notify::NotifyStore::new(&path);
    assert!(notes.list("local", false, 10).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before, "byte for byte what it was");
}
