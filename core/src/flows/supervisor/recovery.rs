//! Recovery: what can be done about a failed step, what may be done without asking, and what is learned.
//!
//! Two kinds of fix, kept strictly apart:
//! - **Temporary** (`extra_patience`): this run only, changes nothing stored. Once it has proven itself
//!   several times for a kind of failure it may be used by an unattended run without asking.
//! - **Permanent**: edits the flow (more retries, carry on past a failure). Always a proposal a person
//!   approves; applied as a new version so it can be undone; its result is checked on the next run.
//!
//! Nothing is applied because it "sounds right": a fix is offered only for failure kinds it can help, is
//! dropped once it stops working, and no fix ever touches credentials, approvals or what a step publishes.

use super::classify::ErrorClass;
use super::verdict::{judge, VerdictKind};
use crate::flows::commands::{self, Command};
use crate::flows::model::{Flow, Node, OnError};
use crate::flows::run::manager::RunObserver;
use crate::flows::run::model::{NodeRun, NodeStatus, Run, RunStatus};
use crate::flows::run::recovery_store::{Knowledge, Recovery};
use crate::flows::run::store::RunStore;
use crate::flows::store::FlowStore;
use crate::flows::validate::validate;
use serde::Serialize;
use std::sync::Arc;

pub type R<T> = Result<T, String>;

pub const EXTRA_PATIENCE: &str = "extra_patience";
pub const MORE_RETRIES: &str = "more_retries";
pub const CARRY_ON: &str = "carry_on";
pub const USE_SIMILAR_ELEMENT: &str = "use_similar_element";

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    pub key: &'static str,
    /// temporary | permanent
    pub kind: &'static str,
    pub title: String,
    pub why: String,
    /// What it changes in the flow, in words. Empty for a temporary fix.
    pub changes: Vec<String>,
    /// The fix changes what the flow does when things go wrong, not just how hard it tries.
    pub changes_logic: bool,
    /// A person must approve it. True for everything permanent.
    pub needs_approval: bool,
    /// Can be applied with `apply`. Temporary fixes are used by the engine, not applied.
    pub applicable: bool,
    pub knowledge: Knowledge,
    #[serde(skip)]
    pub commands: Vec<Command>,
}

/// The fixes worth offering for `class` failing at `node`, most helpful first. Retired ones are left out.
pub fn suggest(store: &RunStore, class: ErrorClass, node: &Node) -> Vec<Suggestion> {
    suggest_with_error(store, class, node, "")
}

/// The one element on a changed page that a browser step probably means: the step names `role:name`, the page
/// no longer has it, and exactly one element of that role is left among those the error lists.
fn similar_element(node: &Node, error: &str) -> Option<String> {
    if node.kind != crate::flows::model::NodeKind::Browser {
        return None;
    }
    let target = node.config.get("target").and_then(|v| v.as_str())?.trim();
    let (role, _) = target.split_once(':').filter(|(r, _)| !r.trim().is_empty() && r.chars().all(|c| c.is_ascii_alphabetic()))?;
    let list = error.split("Similar elements on the page:").nth(1)?;
    let mut same_role = Vec::new();
    for part in list.split(", ") {
        let part = part.trim().trim_end_matches('.');
        if let Some((r, rest)) = part.split_once(' ') {
            if r.eq_ignore_ascii_case(role.trim()) {
                let name = rest.trim().trim_matches('"');
                if !name.is_empty() && !same_role.iter().any(|n: &String| n == name) {
                    same_role.push(name.to_string());
                }
            }
        }
    }
    (same_role.len() == 1).then(|| format!("{}:{}", role.trim().to_lowercase(), same_role.remove(0)))
}

/// Like `suggest`, and where the error says what the page shows now, it can offer to point the step at that.
pub fn suggest_with_error(store: &RunStore, class: ErrorClass, node: &Node, error: &str) -> Vec<Suggestion> {
    let mut out = suggest_inner(store, class, node);
    if class == ErrorClass::WebsiteUiChange {
        if let Some(new_target) = similar_element(node, error) {
            let old = node.config.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let ck = class.key();
            out.insert(
                0,
                Suggestion {
                    key: USE_SIMILAR_ELEMENT,
                    kind: "permanent",
                    title: format!("Point \"{}\" at {new_target} instead", node.name),
                    why: format!("the page no longer has {old}, and {new_target} is the only {} left that could be it", new_target.split(':').next().unwrap_or("element")),
                    changes: vec![format!("\"{}\": element {old} -> {new_target}", node.name)],
                    changes_logic: true,
                    needs_approval: true,
                    applicable: true,
                    knowledge: store.kb_get(USE_SIMILAR_ELEMENT, ck).unwrap_or_else(|_| Knowledge::new(USE_SIMILAR_ELEMENT, ck, 0, 0)),
                    commands: vec![Command::UpdateNode { id: node.id.clone(), name: None, config: Some([("target".to_string(), serde_json::json!(new_target))].into_iter().collect()), enabled: None, credential: None }],
                },
            );
        }
    }
    out.retain(|s| s.knowledge.state != "retired");
    out
}

fn suggest_inner(store: &RunStore, class: ErrorClass, node: &Node) -> Vec<Suggestion> {
    let ck = class.key();
    let kb = |key: &str| store.kb_get(key, ck).unwrap_or_else(|_| Knowledge::new(key, ck, 0, 0));
    let mut out = Vec::new();
    if class.transient() {
        out.push(Suggestion {
            key: EXTRA_PATIENCE,
            kind: "temporary",
            title: "Wait longer and try once more (this run only)".into(),
            why: format!("{}; these often clear by themselves", class.explain()),
            changes: vec![],
            changes_logic: false,
            needs_approval: false,
            applicable: false,
            knowledge: kb(EXTRA_PATIENCE),
            commands: vec![],
        });
        if node.retry.retries < 3 {
            let delay = node.retry.delay_secs.max(10);
            out.push(Suggestion {
                key: MORE_RETRIES,
                kind: "permanent",
                title: format!("Let \"{}\" try again up to 3 times, waiting longer each time", node.name),
                why: format!("it has {} retr{} now, and {}", node.retry.retries, if node.retry.retries == 1 { "y" } else { "ies" }, class.explain()),
                changes: vec![format!("\"{}\": retries {} -> 3, first wait {}s", node.name, node.retry.retries, delay)],
                changes_logic: false,
                needs_approval: true,
                applicable: true,
                knowledge: kb(MORE_RETRIES),
                commands: vec![Command::SetRetry { id: node.id.clone(), retries: 3, delay_secs: delay }],
            });
        }
    }
    if node.on_error != OnError::Continue {
        out.push(Suggestion {
            key: CARRY_ON,
            kind: "permanent",
            title: format!("Carry on if \"{}\" fails", node.name),
            why: "the rest of the flow does not depend on this step".into(),
            changes: vec![format!("\"{}\": on failure, carry on with the next steps", node.name)],
            changes_logic: true,
            needs_approval: true,
            applicable: true,
            knowledge: kb(CARRY_ON),
            commands: vec![Command::SetOnError { id: node.id.clone(), on_error: OnError::Continue }],
        });
    }
    out.retain(|s| s.knowledge.state != "retired");
    out
}

/// What went wrong at a step, in words a person can act on when no fix applies.
pub fn guidance(class: ErrorClass) -> Option<&'static str> {
    match class {
        ErrorClass::Authentication => Some("Sign in to the account again (Aqua never stores or shows the password), then retry the step."),
        ErrorClass::PermissionError => Some("Allow the action in the tool's permission settings, or choose a different folder or account, then retry."),
        ErrorClass::InvalidInput => Some("A setting or value is not valid: open the step, correct it, and retry."),
        ErrorClass::MissingFile => Some("Check that the file or folder exists at the path in the step, then retry."),
        ErrorClass::WebsiteUiChange => Some("The page changed. Open the step to check what it clicks or reads, then retry."),
        _ => None,
    }
}

fn failed_step<'a>(nodes: &'a [NodeRun], node_id: &str) -> Option<&'a NodeRun> {
    nodes.iter().find(|n| n.node_id == node_id && matches!(n.status, NodeStatus::Failed | NodeStatus::NeedsAction | NodeStatus::Interrupted))
}

/// Apply a permanent fix to the flow as a new version. Refuses anything that is not offered for this failure,
/// anything that would leave the flow invalid, and anything for a flow someone edited in the meantime.
pub fn apply(flows: &FlowStore, runs: &RunStore, owner: &str, run: &Run, node_id: &str, key: &str, at: i64) -> R<(Flow, Recovery)> {
    let nodes = runs.node_runs(run.id)?;
    let step = failed_step(&nodes, node_id).ok_or("that step did not fail in this run")?;
    let node = run.def.node(node_id).ok_or("no such step")?;
    let class = super::classify(&step.error);
    let s = suggest_with_error(runs, class, node, &step.error).into_iter().find(|s| s.key == key).ok_or("that fix is not offered for this failure")?;
    if !s.applicable || s.commands.is_empty() {
        return Err("that fix is not applied to the flow".into());
    }
    let current = flows.get(owner, run.flow_id)?.ok_or("no such flow")?;
    let applied = commands::apply(&current.def, &s.commands)?;
    let report = validate(&applied.def);
    if !report.ok() {
        return Err("the fix would leave the flow with problems, so it was not applied".into());
    }
    let updated = flows.update(owner, run.flow_id, applied.def, "supervisor", &format!("Recovery: {}", s.title), Some(current.version), at)?.ok_or("no such flow")?;
    let id = runs.recovery_add(owner, run.flow_id, run.id, node_id, class.key(), key, &s.title, s.kind, current.version, updated.version, at)?;
    let rec = runs.recovery_get(owner, id)?.ok_or("the record was not saved")?;
    Ok((updated, rec))
}

/// Undo a fix: restore the version before it, as a new version. Only while nobody has edited the flow since.
pub fn rollback(flows: &FlowStore, runs: &RunStore, owner: &str, id: i64, at: i64) -> R<Flow> {
    let rec = runs.recovery_get(owner, id)?.ok_or("no such recovery")?;
    if rec.status != "applied" {
        return Err("that fix has already been undone".into());
    }
    let current = flows.get(owner, rec.flow_id)?.ok_or("no such flow")?;
    if current.version != rec.version_after {
        return Err("the flow has been edited since this fix, so it is not undone automatically; restore the version you want from its history".into());
    }
    let restored = flows.restore(owner, rec.flow_id, rec.version_before, "supervisor", at)?.ok_or("no such flow")?;
    runs.recovery_status(id, "rolled_back", at)?;
    if rec.outcome == "pending" {
        runs.recovery_outcome(id, "did_not_help", at)?;
        runs.kb_record(&rec.fix_key, &rec.class, false, at)?;
    }
    Ok(restored)
}

/// Watches finished runs to learn whether fixes worked. The only place the knowledge base changes.
pub struct RecoveryLearner {
    pub runs: Arc<RunStore>,
}

impl RunObserver for RecoveryLearner {
    fn on_run_status(&self, run: &Run, nodes: &[NodeRun], status: RunStatus) {
        if !status.is_finished() || run.dry_run || status == RunStatus::Cancelled {
            return;
        }
        let verdict = judge(run, nodes, &run.def);
        let now = chrono::Utc::now().timestamp();
        let good = matches!(verdict.kind, VerdictKind::Success | VerdictKind::Recovered);
        let bad = verdict.kind == VerdictKind::Failed;
        // A permanent fix is judged by the first run that used the version it produced.
        for rec in self.runs.recoveries_pending(&run.owner_id, run.flow_id).unwrap_or_default() {
            if run.flow_version < rec.version_after || run.id == rec.run_id {
                continue;
            }
            let same_again = bad && verdict.class.is_some_and(|c| c.key() == rec.class) && nodes.iter().any(|n| n.node_id == rec.node_id && n.status == NodeStatus::Failed);
            let outcome = if good {
                "worked"
            } else if same_again {
                "did_not_help"
            } else {
                continue;
            };
            let _ = self.runs.recovery_outcome(rec.id, outcome, now);
            let _ = self.runs.kb_record(&rec.fix_key, &rec.class, good, now);
        }
        // A temporary fix is judged by how the run that used it ended.
        let used: Vec<String> = self.runs.events(&run.owner_id, run.id, 0, 1000).unwrap_or_default().into_iter().filter(|e| e.kind == "temp_fix").map(|e| e.message).collect();
        for m in used {
            let Some(tag) = m.rsplit('[').next().and_then(|t| t.strip_suffix(']')) else { continue };
            let Some((key, class)) = tag.split_once('/') else { continue };
            if good || bad {
                let _ = self.runs.kb_record(key, class, good, now);
            }
        }
    }
}
