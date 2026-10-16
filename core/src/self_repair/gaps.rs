//! Where Aqua notices that it could not do what was asked, and writes it down (card R7-05).
//!
//! These functions are called from a few places that already know the reason (a flow run refused before it starts, a file tool refused a folder). A call is one line there, never
//! changes what that code does, never fails and never waits: with no store installed it returns at once, and an error of the store is logged at debug level and dropped.
//! The facts are written as data. Nothing here sends them to a model, follows them, or turns them into a command.

use super::store::{Gap, GapKind, NewGap};
use crate::flows::model::{FlowDef, NodeKind};
use crate::flows::{platform, registry};
use serde_json::{json, Map, Value};

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Write a gap into the process-wide store. `None` when no store is installed or the write failed (that is logged at debug level and nothing else happens).
pub fn record_gap(g: NewGap) -> Option<Gap> {
    let store = super::installed()?;
    match store.record(g, now()) {
        Ok(gap) => Some(gap),
        Err(e) => {
            tracing::debug!("self repair: a gap was not recorded: {e}");
            None
        }
    }
}

/// Aqua does not know how to do what was asked (`how_to`, card R7-08). `what` is the thing to find out, in a few words ("rotate the pages of a PDF"); the summary is "How to {what}".
/// The summary is the only text a research is ever made from (`research::question_for`). Nothing calls this yet: no place can tell "does not know how" from another failure.
pub fn note_how_to(what: &str, asked: &str, source: &str) -> Option<Gap> {
    let what = what.split_whitespace().collect::<Vec<_>>().join(" ");
    if what.is_empty() {
        return None;
    }
    let mut g = NewGap::new(GapKind::HowTo, what.to_ascii_lowercase(), format!("How to {what}"));
    g.asked = asked.to_string();
    g.source = source.to_string();
    record_gap(g)
}

/// The gap for a step that posts to a platform that is not built yet. `note_refused_steps` writes the same gap (the same key) when a run is refused; the watch (card R7-19) writes
/// it before any run, from a saved flow. `why` is `platform::not_built_note`.
pub(crate) fn not_built_gap(flow_id: i64, flow_name: &str, node: &crate::flows::model::Node, p: &platform::Platform, why: &str) -> NewGap {
    let mut g = NewGap::new(GapKind::PlatformNotBuilt, format!("{flow_id}:{}:{}", node.id, p.id), format!("{} (flow \"{flow_name}\", step \"{}\")", capitalize(why), node.name));
    g.detail = "A real run of this flow would be refused before it started, so nothing would be signed in to, approved or posted. A dry run still works.".into();
    g.source = "watch".into();
    g.flow_id = Some(flow_id);
    g.node_id = node.id.clone();
    g.data = json!({ "platform": p.id, "platform_label": p.label, "flow_id": flow_id, "flow_name": flow_name, "node_id": node.id });
    g
}

/// A flow's real run was refused before it started because some of its steps cannot run (`missing` is what the engine listed). One gap for each such step: a platform that is not
/// built yet (`platform_not_built`), or a kind of step with nothing to run it (`step_not_runnable`). The same step of the same flow refused again is the same gap.
pub fn note_refused_steps(flow_id: i64, flow_name: &str, def: &FlowDef, missing: &[String]) {
    if super::installed().is_none() {
        return;
    }
    for n in def.nodes.iter().filter(|n| n.enabled) {
        // The two sentences below are the ones `Engine::unsupported` writes for the step; matching them keeps this to the steps that list really holds.
        let not_built = (n.kind == NodeKind::SocialMedia)
            .then(|| n.config.get("platform").and_then(|v| v.as_str()).and_then(platform::get))
            .flatten()
            .and_then(|p| platform::not_built_note(p).map(|why| (p, why)))
            .filter(|(_, why)| missing.contains(&format!("\"{}\" ({why})", n.name)));
        if let Some((p, why)) = not_built {
            let mut g = NewGap::new(GapKind::PlatformNotBuilt, format!("{flow_id}:{}:{}", n.id, p.id), format!("{} (flow \"{flow_name}\", step \"{}\")", capitalize(&why), n.name));
            g.detail = "A real run of this flow was refused before it started, so nothing was signed in to, approved or posted. A dry run still works.".into();
            g.asked = format!("Run the flow \"{flow_name}\"");
            g.source = "flow_run".into();
            g.flow_id = Some(flow_id);
            g.node_id = n.id.clone();
            g.data = json!({ "platform": p.id, "platform_label": p.label, "flow_id": flow_id, "flow_name": flow_name, "node_id": n.id });
            record_gap(g);
        } else if missing.contains(&format!("\"{}\" ({})", n.name, registry::spec(n.kind).label)) {
            let mut g = NewGap::new(GapKind::StepNotRunnable, format!("{flow_id}:{}:{}", n.id, n.kind.as_str()), format!("The step \"{}\" ({}) of the flow \"{flow_name}\" has nothing to run it yet", n.name, registry::spec(n.kind).label));
            g.detail = "A real run of this flow was refused before it started. A dry run still works.".into();
            g.asked = format!("Run the flow \"{flow_name}\"");
            g.source = "flow_run".into();
            g.flow_id = Some(flow_id);
            g.node_id = n.id.clone();
            g.data = json!({ "node_kind": n.kind.as_str(), "flow_id": flow_id, "flow_name": flow_name, "node_id": n.id });
            record_gap(g);
        }
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A folder as a key: lower case, forward slashes, no slash at the end. (`E:\Work\` and `e:/work` are one folder.)
pub fn folder_key(path: &str) -> String {
    folder_shown(path).to_ascii_lowercase()
}

/// A folder as it is shown: forward slashes, no slash at the end.
fn folder_shown(path: &str) -> String {
    path.trim().replace('\\', "/").trim_end_matches('/').to_string()
}

/// The folder that holds a path (`E:/x/y.txt` -> `E:/x`); a path straight under a drive gives the drive.
fn parent_folder(path: &str) -> String {
    let p = folder_shown(path);
    match p.rfind('/') {
        Some(0) | None => p,
        Some(i) if p[..i].ends_with(':') => format!("{}/", &p[..i]),
        Some(i) => p[..i].to_string(),
    }
}

/// The file tools that look at a folder: the path they were refused is the folder itself.
const LOOKS_AT_A_FOLDER: [&str; 3] = ["fs.list", "fs.find", "fs.grep"];
/// The file tools that only read: a folder they need is a read-only need.
const READ_TOOLS: [&str; 5] = ["fs.list", "fs.find", "fs.grep", "fs.read", "fs.stat"];

/// What is known about a file call that was refused because its folder is outside the ones Aqua may use.
pub struct FolderRefusal<'a> {
    /// The agent run, or empty for a call outside a run.
    pub run_id: &'a str,
    /// The file tool that was refused (`fs.write`).
    pub tool: &'a str,
    /// The path the call named.
    pub refused: &'a str,
    /// The folder the person's own words named that holds it, when they named one.
    pub named_folder: Option<&'a str>,
    /// `Some(true)`: the person was asked to allow the folder and said no (or did not answer). `Some(false)`: they were asked and the file guard refuses such a place whatever the
    /// answer. `None`: nobody was asked.
    pub person_said_no: Option<bool>,
    pub detail: &'a str,
}

/// A file call was refused for its folder (`folder_not_allowed`). The folder is the one the person named when they did, else the folder of the refused path, and only the first kind is
/// ever turned into an offer to allow it: a path the model chose is a wrong guess, not something the person asked for.
pub fn note_folder_not_allowed(r: FolderRefusal<'_>) {
    if super::installed().is_none() {
        return;
    }
    let (folder, named) = match r.named_folder.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => (folder_shown(f), true),
        None if LOOKS_AT_A_FOLDER.contains(&r.tool) => (folder_shown(r.refused), false),
        None => (parent_folder(r.refused), false),
    };
    if folder.is_empty() {
        return;
    }
    let summary = if named {
        format!("The folder {folder} is outside the folders Aqua may use, and the request named it")
    } else {
        format!("A file tool was refused the folder {folder}: it is outside the folders Aqua may use")
    };
    let mut g = NewGap::new(GapKind::FolderNotAllowed, folder_key(&folder), summary);
    g.detail = r.detail.to_string();
    g.asked = if r.run_id.is_empty() { String::new() } else { crate::sources::goal_of(r.run_id) };
    g.source = if r.run_id.is_empty() { "tool_call" } else { "agent_run" }.into();
    g.run_id = r.run_id.to_string();
    let mut data = Map::new();
    data.insert("folder".into(), json!(folder));
    data.insert("access".into(), json!(if READ_TOOLS.contains(&r.tool) { "ro" } else { "rw" }));
    data.insert("named_by_person".into(), json!(named));
    data.insert("tool".into(), json!(r.tool));
    data.insert("refused_path".into(), json!(r.refused));
    if let Some(no) = r.person_said_no {
        data.insert("refused_by_guard".into(), json!(!no));
    }
    g.data = Value::Object(data);
    record_gap(g);
}

/// The person was asked to allow the folder their request named (`allow` is the answer of `fs.allow_folder`) and it was not allowed: they said no or did not answer, or the file guard
/// never hands such a place out whatever they answer.
pub fn note_folder_ask_failed(run_id: &str, tool: &str, refused: &str, folder: &str, allow: &aqua_tool_host::ToolResult) {
    use aqua_tool_host::ErrorCode as E;
    let said_no = allow.error.as_ref().is_some_and(|e| matches!(e.code, E::ApprovalDenied | E::ApprovalTimeout | E::ApprovalRequired | E::Cancelled));
    let why = allow.error.as_ref().map(|e| e.message.as_str()).unwrap_or("");
    let detail = format!("The person was asked to allow the folder and it was not allowed: {why}");
    note_folder_not_allowed(FolderRefusal { run_id, tool, refused, named_folder: Some(folder), person_said_no: Some(said_no), detail: &detail });
}

/// The path a refused file call named, out of the guard's own sentence (`'E:/x/y.txt' is outside the folders Aqua may use. ...`).
fn refused_path_of(message: &str) -> Option<String> {
    let rest = message.strip_prefix('\'')?;
    let (path, _) = rest.split_once("' is outside the folders Aqua may use")?;
    Some(path.to_string())
}

/// The result of a file call, after everything the agent layer tried: when it is the refusal "outside the folders Aqua may use", write the gap. A call that was allowed after the
/// person said yes is not a gap, and neither is a refusal for any other reason.
pub fn note_refused_file_result(res: &aqua_tool_host::ToolResult, tool: &str, run_id: &str) {
    if res.ok || super::installed().is_none() {
        return;
    }
    let Some(e) = res.error.as_ref() else { return };
    if e.code != aqua_tool_host::ErrorCode::PermissionDenied || !e.message.contains("outside the folders Aqua may use") {
        return;
    }
    let Some(refused) = refused_path_of(&e.message) else { return };
    let named = if run_id.is_empty() { None } else { crate::agent_tools::folder_to_allow(&crate::sources::goal_of(run_id), &refused) };
    note_folder_not_allowed(FolderRefusal { run_id, tool, refused: &refused, named_folder: named.as_deref(), person_said_no: None, detail: &e.message });
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{global, unique};
    use super::*;
    use crate::flows::model::Node;

    fn facebook_flow() -> FlowDef {
        let mut publish = Node::new("publish_facebook", NodeKind::SocialMedia, "Publish to Facebook");
        publish.config = json!({ "platform": "facebook", "action": "publish", "text": "hi" });
        let db = Node::new("save", NodeKind::Database, "Save it");
        FlowDef { name: "Weekly post".into(), nodes: vec![publish, db], ..Default::default() }
    }

    #[test]
    fn folders_are_keyed_without_case_slash_direction_or_a_slash_at_the_end() {
        assert_eq!(folder_key(r"E:\Work\Reports\"), "e:/work/reports");
        assert_eq!(folder_key("e:/work/reports"), folder_key(r"E:\Work\Reports"));
        assert_eq!(parent_folder("E:/vaibhav_testing/hello.py"), "E:/vaibhav_testing");
        assert_eq!(parent_folder(r"E:\a\b\c.txt"), "E:/a/b");
        assert_eq!(parent_folder("E:/y.txt"), "E:/", "a file straight under a drive: the drive");
        assert_eq!(refused_path_of("'E:/x/y.txt' is outside the folders Aqua may use. Use a path inside one of: @ws = C:/x").as_deref(), Some("E:/x/y.txt"));
        assert_eq!(refused_path_of("'@ws' is read-only"), None);
    }

    #[test]
    fn a_flow_refused_for_a_platform_that_is_not_built_is_one_gap_that_counts_and_a_step_with_nothing_to_run_it_is_another() {
        let store = global();
        let (flow_id, name) = (unique(), "Weekly post");
        let def = facebook_flow();
        // What `Engine::unsupported` lists for this flow when nothing runs a database step and no platform but LinkedIn is built.
        let why = platform::not_built_note(platform::get("facebook").unwrap()).unwrap();
        let missing = vec![format!("\"Publish to Facebook\" ({why})"), format!("\"Save it\" ({})", registry::spec(NodeKind::Database).label)];
        note_refused_steps(flow_id, name, &def, &missing);
        note_refused_steps(flow_id, name, &def, &missing);
        let facebook = store.gaps(None, 500).unwrap().into_iter().find(|g| g.flow_id == Some(flow_id) && g.kind == "platform_not_built").expect("the Facebook gap");
        assert_eq!(facebook.count, 2, "the same step refused twice is one gap that counts");
        assert_eq!(facebook.node_id, "publish_facebook");
        assert_eq!(facebook.data["platform"], "facebook");
        assert!(facebook.summary.starts_with("Posting to Facebook is not built yet") && facebook.summary.contains("Weekly post"), "{}", facebook.summary);
        assert_eq!(facebook.status, "new");
        let step = store.gaps(None, 500).unwrap().into_iter().find(|g| g.flow_id == Some(flow_id) && g.kind == "step_not_runnable").expect("the database step gap");
        assert_eq!((step.count, step.data["node_kind"].clone()), (2, json!("database")));
        // A step the engine did not list is not a gap, and neither is one that is switched off.
        let other = unique();
        note_refused_steps(other, name, &def, &[]);
        let mut off = facebook_flow();
        off.nodes[0].enabled = false;
        note_refused_steps(other, name, &off, &missing);
        assert!(store.gaps(None, 500).unwrap().iter().all(|g| g.flow_id != Some(other) || g.kind == "step_not_runnable"), "a platform step that is switched off is not refused");
    }

    #[test]
    fn a_secret_in_a_gap_is_scrubbed_before_it_is_stored() {
        let store = global();
        let key = format!("secret-{}", unique());
        let mut g = NewGap::new(GapKind::Other, key.clone(), "A call failed with token sk-abcdefghijklmnopqrstuvwxyz123456 in it"); // secret-scan: ignore
        g.detail = "password: hunter2 was in the text".into();
        g.data = json!({ "note": "key sk-abcdefghijklmnopqrstuvwxyz123456" }); // secret-scan: ignore
        let gap = record_gap(g).expect("recorded");
        let stored = store.gap(gap.id).unwrap().unwrap();
        let all = format!("{} {} {} {}", stored.summary, stored.detail, stored.data, stored.key);
        assert!(!all.contains("sk-abcdefghijklmnopqrstuvwxyz123456") && !all.contains("hunter2"), "{all}"); // secret-scan: ignore
        assert!(stored.summary.contains("[redacted]"), "{}", stored.summary);
    }
}
