//! How a run really ended. "The run finished" is not "the work is done": a step that failed and was
//! carried past, a publish with no proof, or a run still waiting are all different from success.

use super::classify::{classify, ErrorClass};
use crate::flows::model::FlowDef;
use crate::flows::registry::{self, Risk};
use crate::flows::run::model::{NodeRun, NodeStatus, Run, RunStatus};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    /// Everything that had to happen did, and there is evidence for it.
    Success,
    /// Something went wrong on the way and was put right; the goal was reached.
    Recovered,
    /// The goal was not reached.
    Failed,
    /// It ended, but there is no way to tell whether the goal was reached.
    Unknown,
    /// Not finished: it is waiting for a person or a time.
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub kind: VerdictKind,
    /// One sentence for the person.
    pub reason: String,
    /// What the verdict rests on: the steps and values that show it.
    pub evidence: Vec<String>,
    /// For a failure, what kind.
    pub class: Option<ErrorClass>,
}

fn v(kind: VerdictKind, reason: impl Into<String>, evidence: Vec<String>, class: Option<ErrorClass>) -> Verdict {
    Verdict { kind, reason: reason.into(), evidence, class }
}

/// Proof that an outside action happened: a link, an id, a status that is not a placeholder.
fn has_proof(n: &NodeRun) -> Option<String> {
    for key in ["post_url", "url", "id", "message_id", "file", "path", "status"] {
        if let Some(s) = n.outputs.get(key).and_then(|x| x.as_str()).filter(|s| !s.trim().is_empty() && !s.starts_with("[dry run]")) {
            return Some(format!("{key}: {}", s.chars().take(120).collect::<String>()));
        }
    }
    // A webhook or API that answered with a success code is itself the evidence that the call was accepted.
    if let Some(code) = n.outputs.get("status").and_then(|v| v.as_u64()).filter(|c| (200..300).contains(c)) {
        return Some(format!("status: {code}"));
    }
    if n.note.contains("already done earlier") {
        return Some("recorded as done earlier".into());
    }
    None
}

pub fn judge(run: &Run, nodes: &[NodeRun], def: &FlowDef) -> Verdict {
    let name = |id: &str| def.node(id).map(|n| n.name.clone()).unwrap_or_else(|| id.to_string());
    match run.status {
        RunStatus::Queued | RunStatus::Running | RunStatus::Paused => return v(VerdictKind::Waiting, "It has not finished.", vec![], None),
        RunStatus::Waiting => return v(VerdictKind::Waiting, "It is waiting for you or for a time.", vec![], None),
        RunStatus::Cancelled => return v(VerdictKind::Failed, "It was stopped before it finished.", vec![], None),
        RunStatus::Interrupted => {
            let at = nodes.iter().find(|n| n.status == NodeStatus::Interrupted).map(|n| name(&n.node_id)).unwrap_or_default();
            return v(VerdictKind::Unknown, format!("Aqua stopped while it was running{}; it is not known how far it got.", if at.is_empty() { String::new() } else { format!(" (at \"{at}\")") }), vec![], None);
        }
        RunStatus::Failed => {
            let bad = nodes.iter().find(|n| matches!(n.status, NodeStatus::Failed | NodeStatus::NeedsAction));
            let text = bad.map(|n| n.error.clone()).filter(|e| !e.is_empty()).unwrap_or_else(|| run.error.clone());
            let why = match bad {
                Some(n) => format!("It stopped at \"{}\": {}", name(&n.node_id), text),
                None => format!("It failed: {text}"),
            };
            return v(VerdictKind::Failed, why, vec![], Some(classify(&text)));
        }
        RunStatus::Succeeded => {}
    }
    if run.dry_run {
        return v(VerdictKind::Unknown, "This was a dry run: nothing outside Aqua was done, so nothing is proven.", vec![], None);
    }

    let mut evidence = Vec::new();
    // A step that failed and was carried past means the goal was not reached in full.
    let carried: Vec<&NodeRun> = nodes.iter().filter(|n| n.status == NodeStatus::Failed).collect();
    if let Some(n) = carried.first() {
        let class = classify(&n.error);
        return v(VerdictKind::Failed, format!("It finished, but \"{}\" failed and was skipped past: {}", name(&n.node_id), n.error), vec![], Some(class));
    }
    // Every outside action must show its proof.
    let mut unproven = Vec::new();
    for n in nodes.iter().filter(|n| n.status == NodeStatus::Succeeded) {
        let Some(node) = def.node(&n.node_id) else { continue };
        if registry::spec(node.kind).risk != Risk::High {
            continue;
        }
        match has_proof(n) {
            Some(p) => evidence.push(format!("{}: {p}", node.name)),
            None => unproven.push(node.name.clone()),
        }
    }
    if !unproven.is_empty() {
        return v(VerdictKind::Unknown, format!("It finished, but there is no proof that {} really happened.", unproven.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(" and ")), evidence, None);
    }
    let retried: Vec<&NodeRun> = nodes.iter().filter(|n| n.attempts > 1 && n.status == NodeStatus::Succeeded).collect();
    if !retried.is_empty() {
        let list = retried.iter().map(|n| format!("\"{}\" (attempt {})", name(&n.node_id), n.attempts)).collect::<Vec<_>>().join(", ");
        evidence.push(format!("needed more than one try: {list}"));
        return v(VerdictKind::Recovered, format!("It completed after a step needed more than one try: {list}."), evidence, None);
    }
    let done = nodes.iter().filter(|n| n.status == NodeStatus::Succeeded).count();
    evidence.push(format!("{done} steps completed, none failed"));
    v(VerdictKind::Success, format!("All {done} steps that ran completed."), evidence, None)
}
