//! Fix proposals as data (card R7-09): a proposal is one typed change that would close a gap, with a one-line reason, a dry run in words, and an undo recorded when it is applied.
//!
//! There are four kinds and no more: allow a folder (the tool host asks the person again), switch a flow's platform, switch a flow step off, keep a drafted site recipe as a file (card R7-10: never switched on, never run). A proposal is never code and never text
//! to follow. Nothing here runs by itself: `approve` is called by a route that needs the UI token, so the API token, the voice and the model cannot give an approval.
//! Applying twice is refused; a flow that was edited since the proposal was made is refused with the reason, never forced; every applied change records how to take it back.

use super::store::{Gap, NewProposal, Proposal, RepairError, RepairStore, R};
use crate::flows::commands::{self, Command};
use crate::flows::model::{Flow, FlowDef, NodeKind};
use crate::flows::planner::edit::plan_edit;
use crate::flows::planner::Answers;
use crate::flows::platform;
use crate::flows::store::FlowStore;
use crate::flows::validate::validate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Who a flow edit made here is attributed to, in the flow's version history and audit trail.
pub const ACTOR: &str = "self_repair";

/// The change a proposal makes. It is stored as the proposal's payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    /// Ask the tool host to let the file tools use a folder until Aqua restarts. Applying it asks the person again in the app.
    AllowFolder { path: String, access: String },
    /// Point the flow at another platform, as "switch X to Y" in the flow chat does (every step of the flow that names X). `flow_version` is the version it was proposed for.
    SwitchPlatform { flow_id: i64, node_id: String, from: String, to: String, flow_version: i64 },
    /// Switch one step off; the flow skips it and carries on. `flow_version` is the version it was proposed for.
    SwitchOffStep { flow_id: i64, node_id: String, flow_version: i64 },
    /// Keep a drafted site recipe as a file in the recipe drafts folder (card R7-10). `draft_json` is the draft's own JSON, checked again when this is applied. It is never switched on and
    /// never run: applying it only writes the file, and the undo deletes that file.
    RecipeDraft { draft_json: String },
}

impl Change {
    pub fn kind(&self) -> &'static str {
        match self {
            Change::AllowFolder { .. } => "allow_folder",
            Change::SwitchPlatform { .. } => "switch_platform",
            Change::SwitchOffStep { .. } => "switch_off_step",
            Change::RecipeDraft { .. } => "recipe_draft",
        }
    }
}

/// What a granted folder looked like at the tool host.
pub struct FolderGrant {
    pub text: String,
    /// The host's id of the call, to find it again in the audit log.
    pub request_id: String,
    /// The host's answer for the call (`approved`, `approved_session`, ...) and who gave it (`ui`).
    pub decision: String,
    pub by: String,
    /// False when the folder was already allowed, so nothing changed.
    pub added: bool,
}

/// The tool host as far as folders go. The real one asks the person; a test puts a fake in its place.
pub trait FolderHost: Send + Sync {
    /// Ask to allow the folder. `Err` carries the reason it was not allowed (the person said no, or the file guard never hands such a place out).
    fn allow(&self, path: &str, access: &str) -> Result<FolderGrant, String>;
    /// Take a grant back. The default is a host that cannot (a grant then lasts until Aqua restarts); the running host can, through `fs.revoke_folder` (see [`HostFolders`]).
    fn revoke(&self, _path: &str) -> Result<String, String> {
        Err("the tool host cannot take a folder back before Aqua restarts (a grant lasts until then)".into())
    }
    /// Can this host take a grant back before Aqua restarts? The report lists a grant under "no undo" only when it cannot.
    fn can_revoke(&self) -> bool {
        false
    }
}

/// The running tool host, called as the app (the person's own click), which makes the host ask for the person's answer.
pub struct HostFolders;

impl FolderHost for HostFolders {
    fn allow(&self, path: &str, access: &str) -> Result<FolderGrant, String> {
        let host = crate::agent_tools::host().ok_or_else(|| "the tool host is not running, so a folder cannot be allowed now".to_string())?;
        let context = aqua_tool_host::RequestContext::new(aqua_tool_host::Caller::Ui);
        let res = host.call_blocking(aqua_tool_host::ToolRequest { tool: "fs.allow_folder".into(), arguments: json!({ "path": path, "access": access }), context });
        let (decision, by) = res.approval.as_ref().map(|a| (a.decision.clone(), a.by.clone())).unwrap_or_default();
        if res.ok {
            Ok(FolderGrant { text: res.text.clone(), request_id: res.request_id.clone(), decision, by, added: res.data["added"].as_bool().unwrap_or(true) })
        } else {
            let why = res.error.as_ref().map(|e| e.message.clone()).filter(|m| !m.is_empty()).unwrap_or_else(|| res.text.clone());
            Err(format!("the tool host did not allow the folder: {why}"))
        }
    }

    fn revoke(&self, path: &str) -> Result<String, String> {
        let host = crate::agent_tools::host().ok_or_else(|| "the tool host is not running, so a folder cannot be taken back now".to_string())?;
        revoke_on(host, path)
    }

    fn can_revoke(&self) -> bool {
        crate::agent_tools::host().is_some_and(|h| h.manifests().iter().any(|m| m.name == "fs.revoke_folder"))
    }
}

/// Take a folder back through a tool host, as the app (the person's own click: this is the undo of an approved proposal). Taking access away asks nothing. A folder that is not allowed
/// any more (Aqua restarted since, which forgets every grant, or it was taken back already) is the state the undo wants, so that is a success that says so; a folder the settings give, or
/// a host that refuses, is an error.
pub fn revoke_on(host: &aqua_tool_host::ToolHost, path: &str) -> Result<String, String> {
    let context = aqua_tool_host::RequestContext::new(aqua_tool_host::Caller::Ui);
    let res = host.call_blocking(aqua_tool_host::ToolRequest { tool: "fs.revoke_folder".into(), arguments: json!({ "path": path }), context });
    if res.ok {
        return Ok(res.text.clone());
    }
    let error = res.error.as_ref();
    if error.is_some_and(|e| e.code == aqua_tool_host::ErrorCode::NotFound && e.message.starts_with("no folder is allowed by")) {
        return Ok(format!("The folder {path} is not allowed any more (Aqua restarted since, or it was taken back already), so there was nothing to take back."));
    }
    let why = error.map(|e| e.message.clone()).filter(|m| !m.is_empty()).unwrap_or_else(|| res.text.clone());
    Err(format!("the tool host did not take the folder back: {why}"))
}

/// What applying and undoing need.
pub struct Env<'a> {
    pub flows: &'a FlowStore,
    pub folders: &'a dyn FolderHost,
    pub owner: &'a str,
    pub now: i64,
}

fn flow_err(e: String) -> RepairError {
    RepairError::Store(e)
}

/// The proposals made for a gap, and a word when there are none.
pub struct Proposed {
    pub proposals: Vec<Proposal>,
    pub note: String,
}

struct Candidate {
    change: Change,
    title: String,
    reason: String,
    dry_run: String,
}

/// The commands that move the flow from one platform to another, and the notes that go with them. This is the flow builder's own rule ("Replace X with Y"), asked through its public
/// entry point, so a switch here changes exactly what the same words in the flow chat would.
fn switch_platform_plan(def: &FlowDef, node_id: &str, from: &str, to: &str) -> Result<(Vec<Command>, Vec<String>), String> {
    let (Some(f), Some(t)) = (platform::get(from), platform::get(to)) else { return Err("that is not a platform Aqua knows".into()) };
    let node = def.node(node_id).ok_or_else(|| format!("the flow has no step \"{node_id}\" any more"))?;
    if node.kind != NodeKind::SocialMedia || node.config.get("platform").and_then(|v| v.as_str()) != Some(f.id) {
        return Err(format!("the step \"{}\" no longer posts to {}", node.name, f.label));
    }
    if platform::not_built_note(f).is_none() {
        return Err(format!("posting to {} works now, so there is nothing to switch", f.label));
    }
    if platform::not_built_note(t).is_some() {
        return Err(format!("posting to {} is not built either", t.label));
    }
    // The flow builder's rule changes every step that names the platform, and so every step that posts to it. With several posting steps they would all change, though the person
    // approved the switch for one: that is a job for the flow editor, where each step can be seen, and not a one-click fix.
    if posting_steps(def, f.id) > 1 {
        return Err(several_places(f.label));
    }
    let text = format!("Replace {} with {}", f.aliases[0], t.aliases[0]);
    let plan = plan_edit(def, &text, &Answers::new(), chrono::Utc::now().naive_utc());
    if plan.commands.is_empty() {
        return Err(plan.warnings.first().cloned().unwrap_or_else(|| "the flow builder had no change to make".into()));
    }
    let mut notes = plan.explanation;
    notes.extend(plan.warnings);
    Ok((plan.commands, notes))
}

/// How many steps of the flow post to the platform (a switched-off one counts: the flow builder's rule changes it too).
fn posting_steps(def: &FlowDef, platform_id: &str) -> usize {
    def.nodes.iter().filter(|n| n.kind == NodeKind::SocialMedia && n.config.get("platform").and_then(|v| v.as_str()) == Some(platform_id)).count()
}

/// Why a switch of platform is refused for a flow that posts to the platform in several steps. The same words are the note of the proposals that were not made.
pub fn several_places(label: &str) -> String {
    format!("this flow posts to {label} in more than one place: switch it from the flow editor")
}

fn switch_off_command(node_id: &str) -> Command {
    Command::UpdateNode { id: node_id.to_string(), name: None, config: None, enabled: Some(false), credential: None }
}

fn version_moved(flow: &Flow, proposed_for: i64) -> Option<String> {
    (flow.version != proposed_for).then(|| format!("the flow has moved on: it is at version {} and this was proposed for version {proposed_for}; ask for a new proposal", flow.version))
}

fn candidates(env: &Env<'_>, gap: &Gap) -> Result<(Vec<Candidate>, String), RepairError> {
    match gap.kind.as_str() {
        "platform_not_built" => {
            let flow_id = gap.flow_id.ok_or_else(|| RepairError::Refused("this gap names no flow".into()))?;
            let from = platform::get(gap.data["platform"].as_str().unwrap_or("")).ok_or_else(|| RepairError::Refused("this gap names no platform Aqua knows".into()))?;
            let Some(flow) = env.flows.get(env.owner, flow_id).map_err(flow_err)? else { return Err(RepairError::Refused("the flow is gone, so there is nothing to change".into())) };
            let Some(why) = platform::not_built_note(from) else { return Err(RepairError::Refused(format!("posting to {} works now, so there is nothing to change", from.label))) };
            let node = flow.def.node(&gap.node_id).filter(|n| n.kind == NodeKind::SocialMedia && n.config.get("platform").and_then(|v| v.as_str()) == Some(from.id));
            let Some(node) = node else { return Err(RepairError::Refused(format!("the flow no longer has that step posting to {}", from.label))) };
            let mut out = Vec::new();
            let mut several = false;
            for to in platform::publishable_ids().into_iter().filter_map(platform::get).filter(|p| p.id != from.id) {
                // A switch the flow builder cannot make is simply not offered.
                let plan = switch_platform_plan(&flow.def, &gap.node_id, from.id, to.id);
                several |= plan.as_ref().is_err_and(|why| *why == several_places(from.label));
                if let Ok((_, notes)) = plan {
                    out.push(Candidate {
                        change: Change::SwitchPlatform { flow_id, node_id: gap.node_id.clone(), from: from.id.into(), to: to.id.into(), flow_version: flow.version },
                        title: format!("Post to {} instead of {} in \"{}\"", to.label, from.label, flow.def.name),
                        reason: format!("{}; a real run of this flow is refused before it starts.", capitalize(&why)),
                        dry_run: format!(
                            "Would change the flow \"{}\" (now version {}) so that it posts to {} instead of {}. {} This makes a new version, written by {ACTOR}; undoing it restores version {}.",
                            flow.def.name,
                            flow.version,
                            to.label,
                            from.label,
                            notes.join(" "),
                            flow.version
                        ),
                    });
                }
            }
            if node.enabled {
                out.push(Candidate {
                    change: Change::SwitchOffStep { flow_id, node_id: gap.node_id.clone(), flow_version: flow.version },
                    title: format!("Switch off the step \"{}\" in \"{}\"", node.name, flow.def.name),
                    reason: format!("{}; with the step off the flow skips it and carries on.", capitalize(&why)),
                    dry_run: format!(
                        "Would switch off the step \"{}\" in the flow \"{}\" (now version {}). The flow skips a switched-off step and carries on. This makes a new version, written by {ACTOR}; undoing it restores version {}.",
                        node.name, flow.def.name, flow.version, flow.version
                    ),
                });
            }
            let note = if several { format!("No switch to another platform is offered: {}.", several_places(from.label)) } else { String::new() };
            Ok((out, note))
        }
        "folder_not_allowed" => {
            if gap.data["named_by_person"].as_bool() != Some(true) {
                return Ok((Vec::new(), "The request did not name this folder, so Aqua does not offer to allow it (it only offers a folder that the person's own words named).".into()));
            }
            if gap.data["refused_by_guard"].as_bool() == Some(true) {
                return Ok((Vec::new(), "Aqua never hands this kind of place out (Windows, programs, credentials, Aqua's own data), so no proposal is made.".into()));
            }
            let folder = gap.data["folder"].as_str().unwrap_or("").trim().to_string();
            if folder.is_empty() {
                return Err(RepairError::Refused("this gap names no folder".into()));
            }
            let access = if gap.data["access"].as_str() == Some("ro") { "ro" } else { "rw" };
            let what = if access == "rw" { "read and write files" } else { "read files" };
            Ok((
                vec![Candidate {
                    change: Change::AllowFolder { path: folder.clone(), access: access.into() },
                    title: format!("Allow Aqua's file tools to use {folder}"),
                    reason: "A request named this folder and it is outside the folders Aqua may use.".into(),
                    dry_run: format!("Would ask you again in the app: allow Aqua's file tools to {what} in the folder {folder} until Aqua restarts. Each change there would still ask you. Nothing changes until you answer."),
                }],
                String::new(),
            ))
        }
        _ => Ok((Vec::new(), "Aqua has no fix to offer for this kind of gap yet.".into())),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Make the proposals a gap suggests: a platform that is not built gives a switch to a platform that is, and a switch-off of the step; a folder the person named gives an offer to allow
/// it; the other kinds give none. Asking again returns the proposals that are still pending and the same; a pending proposal that no longer matches (the flow changed) is rejected as
/// replaced. Nothing is applied.
pub fn create_proposals_for(store: &RepairStore, env: &Env<'_>, gap_id: i64) -> R<Proposed> {
    let gap = store.gap(gap_id)?.ok_or_else(|| RepairError::NotFound("no such gap".into()))?;
    if gap.status == "dismissed" {
        return Err(RepairError::Refused("this gap was dismissed; nothing is proposed for it".into()));
    }
    let (wanted, note) = candidates(env, &gap)?;
    let pending = store.proposals(Some("pending"), Some(gap_id), 100)?;
    let mut out: Vec<Proposal> = Vec::new();
    for c in &wanted {
        let payload = serde_json::to_value(&c.change).map_err(|e| RepairError::Store(format!("self repair store: {e}")))?;
        match pending.iter().find(|p| p.payload == payload) {
            Some(same) => out.push(same.clone()),
            None => out.push(store.proposal_add(NewProposal { gap_id, kind: c.change.kind().into(), title: c.title.clone(), reason: c.reason.clone(), dry_run: c.dry_run.clone(), payload }, env.now)?),
        }
    }
    // A drafted recipe (card R7-10) is offered by `propose_recipe_draft`, not made from the gap, so asking for the gap's own fixes never rejects it as "replaced".
    for old in pending.iter().filter(|p| p.kind != "recipe_draft" && !out.iter().any(|o| o.id == p.id)) {
        store.proposal_reject(old.id, "replaced by a newer proposal")?;
    }
    store.refresh_gap_status(gap_id)?;
    Ok(Proposed { proposals: out, note })
}

/// Offer a drafted site recipe to the person (card R7-10). The gap is "no recipe exists for this site and goal", one row for each, so offering the same draft again is the same gap and the same
/// pending proposal. A draft that breaks a rule of `RecipeDraft::validate` is refused. Nothing is stored until the person approves in the app, and approving only keeps the draft as a file: it is
/// never switched on and never run (`recipe_builder::drafts`).
pub fn propose_recipe_draft(store: &RepairStore, now: i64, draft: &crate::flows::recipe_builder::RecipeDraft) -> R<Proposal> {
    draft.validate().map_err(|why| RepairError::Refused(format!("this draft cannot be offered: {why}")))?;
    let mut new_gap = super::store::NewGap::new(super::store::GapKind::Other, format!("recipe:{}:{}", draft.site, draft.goal.to_lowercase()), format!("No recipe exists for {} ({})", draft.site, draft.goal));
    new_gap.detail = "A recipe was drafted from what the page shows. It has never been tried on the real site.".into();
    new_gap.asked = draft.goal.clone();
    new_gap.source = "recipe_builder".into();
    new_gap.data = json!({ "site": draft.site, "goal": draft.goal, "recipe_wanted": true });
    let gap = store.record(new_gap, now)?;
    if gap.status == "dismissed" {
        return Err(RepairError::Refused("this gap was dismissed; nothing is proposed for it".into()));
    }
    let change = Change::RecipeDraft { draft_json: draft.to_json() };
    let payload = serde_json::to_value(&change).map_err(|e| RepairError::Store(format!("self repair store: {e}")))?;
    if let Some(same) = store.proposals(Some("pending"), Some(gap.id), 100)?.into_iter().find(|p| p.payload == payload) {
        return Ok(same);
    }
    let first_send = draft.sends().first().map(|i| draft.steps[*i].describe_plain()).unwrap_or_else(|| "no step sends anything".into());
    let proposal = store.proposal_add(
        NewProposal {
            gap_id: gap.id,
            kind: change.kind().into(),
            title: format!("Keep a draft recipe for {} ({})", draft.site, draft.goal),
            reason: "No recipe exists for this site. This draft was made by reading the page, never by using it, and has never been tried on the real site.".into(),
            dry_run: format!(
                "Would keep this draft as a file in the recipe drafts folder (data/recipes/drafts). It would not be switched on and nothing would run it. The step that sends something is: {first_send}. Undoing it deletes the file.\n\n{}",
                draft.render()
            ),
            payload,
        },
        now,
    )?;
    store.refresh_gap_status(gap.id)?;
    Ok(proposal)
}

struct Applied {
    undo: Value,
    result: Value,
}

/// Make the edit to the flow `flow` as a new version, if it leaves the flow no worse.
fn edit_flow(env: &Env<'_>, flow: &Flow, commands: &[Command], note: &str) -> Result<Applied, String> {
    let applied = commands::apply(&flow.def, commands)?;
    if validate(&applied.def).errors().count() > validate(&flow.def).errors().count() {
        return Err("the change would leave the flow with problems, so it was not applied".into());
    }
    let updated = env.flows.update(env.owner, flow.id, applied.def, ACTOR, note, Some(flow.version), env.now)?.ok_or("the flow no longer exists")?;
    if updated.version == flow.version {
        return Err("the change made no difference to the flow".into());
    }
    Ok(Applied {
        undo: json!({ "kind": "flow_version", "flow_id": flow.id, "restore_version": flow.version, "applied_version": updated.version }),
        result: json!({ "flow_id": flow.id, "version": updated.version, "notes": applied.notes }),
    })
}

fn current_flow(env: &Env<'_>, flow_id: i64) -> Result<Flow, String> {
    env.flows.get(env.owner, flow_id)?.ok_or_else(|| "the flow no longer exists".to_string())
}

fn apply_change(env: &Env<'_>, change: &Change) -> Result<Applied, String> {
    match change {
        Change::AllowFolder { path, access } => {
            let grant = env.folders.allow(path, access)?;
            let undo = if grant.added { json!({ "kind": "revoke_folder", "path": path, "access": access }) } else { json!({ "kind": "nothing", "why": "the folder was already allowed, so nothing was changed" }) };
            Ok(Applied { undo, result: json!({ "text": grant.text, "request_id": grant.request_id, "host_decision": grant.decision, "host_by": grant.by, "added": grant.added }) })
        }
        Change::SwitchPlatform { flow_id, node_id, from, to, flow_version } => {
            let flow = current_flow(env, *flow_id)?;
            if let Some(why) = version_moved(&flow, *flow_version) {
                return Err(why);
            }
            let (commands, _) = switch_platform_plan(&flow.def, node_id, from, to)?;
            edit_flow(env, &flow, &commands, &format!("Self repair: post to {to} instead of {from}"))
        }
        Change::SwitchOffStep { flow_id, node_id, flow_version } => {
            let flow = current_flow(env, *flow_id)?;
            if let Some(why) = version_moved(&flow, *flow_version) {
                return Err(why);
            }
            let node = flow.def.node(node_id).ok_or_else(|| format!("the flow has no step \"{node_id}\" any more"))?;
            if !node.enabled {
                return Err(format!("the step \"{}\" is already switched off", node.name));
            }
            edit_flow(env, &flow, &[switch_off_command(node_id)], &format!("Self repair: switch off the step \"{}\"", node.name))
        }
        Change::RecipeDraft { draft_json } => {
            let kept = crate::flows::recipe_builder::drafts::keep_draft(draft_json)?;
            Ok(Applied { undo: kept.undo, result: kept.result })
        }
    }
}

fn undo_change(env: &Env<'_>, undo: &Value) -> Result<String, String> {
    match undo["kind"].as_str() {
        Some("flow_version") => {
            let (Some(flow_id), Some(restore), Some(applied)) = (undo["flow_id"].as_i64(), undo["restore_version"].as_i64(), undo["applied_version"].as_i64()) else { return Err("the undo record is damaged".into()) };
            let flow = current_flow(env, flow_id)?;
            if flow.version != applied {
                return Err("the flow has been edited since this change, so it is not undone automatically; restore the version you want from its history".into());
            }
            let restored = env.flows.restore(env.owner, flow_id, restore, ACTOR, env.now)?.ok_or("that version of the flow is no longer kept")?;
            Ok(format!("Restored version {restore} of the flow (now version {})", restored.version))
        }
        Some("revoke_folder") => env.folders.revoke(undo["path"].as_str().unwrap_or("")),
        Some("nothing") => Ok("Nothing to undo: the folder was already allowed before.".into()),
        Some("delete_recipe_draft" | "draft_already_kept") => crate::flows::recipe_builder::drafts::undo_kept(undo),
        _ => Err("this change has no undo".into()),
    }
}

/// Approve a pending proposal and apply it. Returns the proposal as it stands afterwards: `applied`, or `failed` with the reason in `error` (nothing was changed). Only a pending
/// proposal can be approved, and for exactly one caller: the same proposal again is refused. `by` is who approved (`ui`: the person, in the app).
pub fn approve(store: &RepairStore, env: &Env<'_>, id: i64, by: &str) -> R<Proposal> {
    let p = store.proposal(id)?.ok_or_else(|| RepairError::NotFound("no such proposal".into()))?;
    if !store.proposal_claim(id, by, env.now)? {
        return Err(RepairError::Refused(format!("this proposal is {}; only a pending proposal can be approved", p.status)));
    }
    let outcome = match serde_json::from_value::<Change>(p.payload.clone()) {
        Ok(change) => apply_change(env, &change),
        Err(e) => Err(format!("the proposal could not be read: {e}")),
    };
    match outcome {
        Ok(applied) => store.proposal_applied(id, env.now, &applied.undo, &applied.result)?,
        Err(reason) => store.proposal_failed(id, &reason)?,
    }
    store.refresh_gap_status(p.gap_id)?;
    store.proposal(id)?.ok_or_else(|| RepairError::Store("self repair store: the proposal was lost".into()))
}

/// Say no to a pending proposal. Nothing is changed anywhere.
pub fn reject(store: &RepairStore, id: i64) -> R<Proposal> {
    let p = store.proposal(id)?.ok_or_else(|| RepairError::NotFound("no such proposal".into()))?;
    if !store.proposal_reject(id, "rejected by the person")? {
        return Err(RepairError::Refused(format!("this proposal is {}; only a pending proposal can be rejected", p.status)));
    }
    store.refresh_gap_status(p.gap_id)?;
    store.proposal(id)?.ok_or_else(|| RepairError::Store("self repair store: the proposal was lost".into()))
}

/// Take an applied proposal back, with the undo that was recorded when it was applied. A flow that was edited since is not touched (the reason says so); a change with no way back says so.
pub fn undo(store: &RepairStore, env: &Env<'_>, id: i64) -> R<Proposal> {
    let p = store.proposal(id)?.ok_or_else(|| RepairError::NotFound("no such proposal".into()))?;
    if p.status != "applied" {
        return Err(RepairError::Refused(format!("this proposal is {}; only an applied proposal can be undone", p.status)));
    }
    let text = undo_change(env, &p.undo).map_err(RepairError::Refused)?;
    let mut result = p.result.clone();
    if let Some(o) = result.as_object_mut() {
        o.insert("undo".into(), json!(text));
    }
    store.proposal_undone(id, env.now, &result)?;
    store.refresh_gap_status(p.gap_id)?;
    store.proposal(id)?.ok_or_else(|| RepairError::Store("self repair store: the proposal was lost".into()))
}

/// Dismiss a gap: the person does not want to see it. Its pending proposals are rejected with it. The count still grows if it happens again.
pub fn dismiss_gap(store: &RepairStore, gap_id: i64) -> R<Gap> {
    store.gap(gap_id)?.ok_or_else(|| RepairError::NotFound("no such gap".into()))?;
    for p in store.proposals(Some("pending"), Some(gap_id), 100)? {
        store.proposal_reject(p.id, "the gap was dismissed")?;
    }
    store.set_gap_status(gap_id, "dismissed")?;
    store.gap(gap_id)?.ok_or_else(|| RepairError::NotFound("no such gap".into()))
}
