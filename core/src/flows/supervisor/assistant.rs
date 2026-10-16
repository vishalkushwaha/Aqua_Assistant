//! Asking about your workflows in plain words: "Did my scheduled workflow run?", "Fix the last error",
//! "Check my workflows and fix any problems you can".
//!
//! Every answer is built from the run history, the schedule records and the health computed from them, never
//! from a model's memory of what "probably" happened. What it may do by itself is small and safe (it can ask
//! the watchdog to clear runs that are stuck); anything that changes a flow comes back as a proposal for the
//! person to approve.

use super::classify::ErrorClass;
use super::health::{health_of, judged_runs, HealthState, Judged, WINDOW};
use super::recovery::{guidance, suggest_with_error};
use super::verdict::VerdictKind;
use crate::flows::run::store::RunStore;
use crate::flows::sched::store::SchedStore;
use crate::flows::sched::tick::Scheduler;
use crate::flows::store::FlowStore;
use crate::flows::zone::Zone;
use serde::Serialize;

pub struct Ctx<'a> {
    pub flows: &'a FlowStore,
    pub runs: &'a RunStore,
    pub sched: &'a SchedStore,
    pub scheduler: Option<&'a Scheduler>,
    pub owner: &'a str,
    pub now: i64,
}

/// A fix the person can approve. Applying it is `recovery::apply`, through the same checks as any other.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Proposal {
    pub run_id: i64,
    pub flow_id: i64,
    pub flow_name: String,
    pub node_id: String,
    pub key: String,
    pub title: String,
    pub changes_logic: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Reply {
    pub text: String,
    pub proposals: Vec<Proposal>,
    /// What was done, if anything. Empty when the answer only reads.
    pub did: Vec<String>,
}

fn reply(text: impl Into<String>) -> Reply {
    Reply { text: text.into(), proposals: vec![], did: vec![] }
}

fn has(t: &str, words: &[&str]) -> bool {
    words.iter().any(|w| t.contains(w))
}

/// Which flow the sentence is about: the one whose name it mentions most fully.
fn flow_named(t: &str, flows: &[(i64, String)]) -> Option<(i64, String)> {
    let words = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).map(String::from).collect::<Vec<_>>();
    let text = words(t);
    let mut best: Option<(f64, &(i64, String))> = None;
    for f in flows {
        let name = words(&f.1);
        if name.is_empty() {
            continue;
        }
        let hit = name.iter().filter(|w| text.contains(w)).count();
        let score = hit as f64 / name.len() as f64;
        if hit > 0 && score >= 0.5 && best.as_ref().is_none_or(|b| score > b.0) {
            best = Some((score, f));
        }
    }
    best.map(|(_, f)| f.clone())
}

fn ago(now: i64, at: i64) -> String {
    let d = (now - at).max(0);
    match d {
        0..=89 => "just now".into(),
        90..=5399 => format!("{} minutes ago", d / 60),
        5400..=129_599 => format!("{} hours ago", (d + 1800) / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

fn state_word(s: HealthState) -> &'static str {
    match s {
        HealthState::Healthy => "healthy",
        HealthState::Degraded => "having some trouble",
        HealthState::Failing => "failing",
        HealthState::Idle => "not run yet",
    }
}

/// Answer, or `None` if this is not a question about workflows (so the caller can treat it as something else).
pub fn answer(ctx: &Ctx, text: &str) -> Option<Reply> {
    let t = text.to_lowercase().replace(['\u{2019}', '\''], "");
    let flows: Vec<(i64, String)> = ctx.flows.list(ctx.owner).ok()?.into_iter().map(|f| (f.id, f.name)).collect();
    let fix = has(&t, &["fix", "repair", "sort out", "resolve"]);
    let named = flow_named(&t, &flows);

    if fix && (has(&t, &["everything", "all my", "any problem", "any problems", "all problems", "check my"]) || (has(&t, &["check"]) && has(&t, &["workflow", "flows"]))) {
        return Some(check_and_fix(ctx, &flows));
    }
    if fix && has(&t, &["last error", "last failure", "last problem", "latest error", "what went wrong", "the error", "the failure", "failed", "problem"]) {
        return Some(fix_last(ctx, &flows, named.map(|n| n.0)));
    }
    let why = has(&t, &["why"]) && has(&t, &["not run", "didnt run", "did not run", "not start", "didnt start", "didnt happen", "never ran", "not happen"]);
    if why || (has(&t, &["did", "has", "have"]) && has(&t, &[" run", "ran ", "run?", "executed", "happen"])) && !fix {
        return Some(did_it_run(ctx, &flows, named.map(|n| n.0), why));
    }
    if has(&t, &["error", "errors", "what failed", "failures"]) && !fix {
        return Some(recent_errors(ctx, &flows));
    }
    if has(&t, &["health", "status", "how are my", "how is", "any problem", "any problems", "are my workflows", "everything ok", "everything okay", "working"]) {
        return Some(status(ctx, &flows, named.map(|n| n.0)));
    }
    None
}

fn all_judged(ctx: &Ctx, flows: &[(i64, String)]) -> Vec<(i64, String, Vec<Judged>)> {
    flows.iter().map(|(id, name)| (*id, name.clone(), judged_runs(ctx.runs, ctx.owner, *id, WINDOW).unwrap_or_default())).collect()
}

fn status(ctx: &Ctx, flows: &[(i64, String)], only: Option<i64>) -> Reply {
    let mut lines = Vec::new();
    for (id, name, judged) in all_judged(ctx, flows).into_iter().filter(|(id, _, _)| only.is_none_or(|o| o == *id)) {
        let h = health_of(&name, id, &judged);
        let score = h.score.map(|s| format!(" ({s}/100)")).unwrap_or_default();
        let last = if h.last_run_at > 0 { format!("; last run {}", ago(ctx.now, h.last_run_at)) } else { String::new() };
        lines.push(format!("• {name}: {}{score}{last}. {}", state_word(h.state), h.reasons.first().cloned().unwrap_or_default()));
    }
    if lines.is_empty() {
        return reply("You have no workflows yet.");
    }
    reply(lines.join("\n"))
}

fn did_it_run(ctx: &Ctx, flows: &[(i64, String)], only: Option<i64>, why: bool) -> Reply {
    let mut lines = Vec::new();
    let scheds = ctx.sched.list(ctx.owner, only).unwrap_or_default();
    if scheds.is_empty() {
        return reply(match only {
            Some(id) => format!("\"{}\" has no schedule, so it only runs when you start it. Its latest runs are in its Run tab.", flows.iter().find(|f| f.0 == id).map(|f| f.1.clone()).unwrap_or_default()),
            None => "None of your workflows is scheduled.".to_string(),
        });
    }
    for s in scheds {
        let zone = s.zone().unwrap_or(Zone::Local);
        let occ = ctx.sched.occurrences(ctx.owner, s.id, 1).unwrap_or_default();
        let head = format!("• {}", s.name);
        let Some(o) = occ.first() else {
            lines.push(format!("{head}: nothing has come due yet.{}", s.next_run_at.map(|n| format!(" Next: {}.", zone.show(n))).unwrap_or_default()));
            continue;
        };
        let when = zone.show(o.due_at);
        let line = match (o.outcome.as_str(), o.run_id) {
            ("started" | "started_late" | "resolved_run", Some(rid)) => {
                let how = ctx.runs.get_run(ctx.owner, rid).ok().flatten().map(|r| {
                    let nodes = ctx.runs.node_runs(rid).unwrap_or_default();
                    let v = super::verdict::judge(&r, &nodes, &r.def);
                    match v.kind {
                        VerdictKind::Success => "and it worked".to_string(),
                        VerdictKind::Recovered => format!("and it worked after recovering ({})", v.reason.trim_end_matches('.')),
                        VerdictKind::Failed => format!("but it did not work: {}", v.reason),
                        VerdictKind::Unknown => format!("but I could not confirm it worked: {}", v.reason),
                        VerdictKind::Waiting => "and it is still going or waiting for you".to_string(),
                    }
                });
                format!("Yes: the {when} run started{} (run #{rid}) {}.", if o.outcome == "started_late" { " late" } else { "" }, how.unwrap_or_default())
            }
            ("skipped" | "resolved_skip", _) => format!("No: the {when} run was skipped: {}.", o.reason),
            ("missed_pending", _) => format!("No: the {when} run was missed and is waiting for your decision ({}).", o.reason),
            _ => format!("No: the {when} run could not start: {}.", o.reason),
        };
        lines.push(format!("{head}: {line}{}", if !s.enabled { " The schedule is switched off." } else { "" }));
    }
    let _ = why;
    reply(lines.join("\n"))
}

fn recent_errors(ctx: &Ctx, flows: &[(i64, String)]) -> Reply {
    let all: Vec<(String, Vec<Judged>)> = all_judged(ctx, flows).into_iter().map(|(_, n, j)| (n, j)).collect();
    let groups = super::health::error_groups(&all, ctx.now - 7 * 86_400);
    if groups.is_empty() {
        return reply("No failures in the last 7 days.");
    }
    let lines: Vec<String> = groups.iter().map(|g| format!("• {} × {} ({}), in {}. Latest {}.", g.count, g.label, g.explain, g.flows.join(", "), ago(ctx.now, g.last_at))).collect();
    reply(format!("Failures in the last 7 days:\n{}", lines.join("\n")))
}

/// The newest failed run among `flows` (or of one flow), with its judged record.
fn latest_failure(ctx: &Ctx, flows: &[(i64, String)], only: Option<i64>) -> Option<(String, Judged)> {
    let mut best: Option<(String, Judged)> = None;
    for (id, name, judged) in all_judged(ctx, flows).into_iter().filter(|(id, _, _)| only.is_none_or(|o| o == *id)) {
        let _ = id;
        // Only a failure that is still the latest word on that flow needs fixing.
        if let Some(j) = judged.into_iter().next().filter(|j| j.verdict.kind == VerdictKind::Failed) {
            if best.as_ref().is_none_or(|(_, b)| j.run.id > b.run.id) {
                best = Some((name, j));
            }
        }
    }
    best
}

fn proposals_for(ctx: &Ctx, name: &str, j: &Judged) -> (Vec<Proposal>, Option<&'static str>, String) {
    let nodes = ctx.runs.node_runs(j.run.id).unwrap_or_default();
    let mut out = Vec::new();
    let mut guide = None;
    let mut what = String::new();
    for n in nodes.iter().filter(|n| matches!(n.status, crate::flows::run::model::NodeStatus::Failed | crate::flows::run::model::NodeStatus::NeedsAction)) {
        let Some(node) = j.run.def.node(&n.node_id) else { continue };
        let class = super::classify(&n.error);
        what = format!("\"{}\" failed: {} ({}).", node.name, class.label(), class.explain());
        guide = guidance(class);
        for s in suggest_with_error(ctx.runs, class, node, &n.error).into_iter().filter(|s| s.applicable) {
            out.push(Proposal { run_id: j.run.id, flow_id: j.run.flow_id, flow_name: name.into(), node_id: node.id.clone(), key: s.key.into(), title: s.title, changes_logic: s.changes_logic });
        }
        break;
    }
    (out, guide, what)
}

fn fix_last(ctx: &Ctx, flows: &[(i64, String)], only: Option<i64>) -> Reply {
    let Some((name, j)) = latest_failure(ctx, flows, only) else {
        return reply(match only {
            Some(_) => "That workflow's latest run did not fail, so there is nothing to fix.".to_string(),
            None => "Nothing to fix: no workflow's latest run failed.".to_string(),
        });
    };
    let (proposals, guide, what) = proposals_for(ctx, &name, &j);
    let mut text = format!("The last failure was in \"{name}\" (run #{}, {}). {what}", j.run.id, ago(ctx.now, j.run.finished_at));
    if let Some(g) = guide {
        text.push_str(&format!("\n{g}"));
    }
    if proposals.is_empty() {
        text.push_str("\nThere is no change I can safely make for this myself. Once you have dealt with the cause, run it again.");
    } else {
        text.push_str("\nI can make these changes if you approve (nothing is changed until then):");
        for p in &proposals {
            text.push_str(&format!("\n• {}{}", p.title, if p.changes_logic { " (changes what the flow does when a step fails)" } else { "" }));
        }
    }
    Reply { text, proposals, did: vec![] }
}

fn check_and_fix(ctx: &Ctx, flows: &[(i64, String)]) -> Reply {
    let mut did = Vec::new();
    if let Some(s) = ctx.scheduler {
        let w = s.watchdog(ctx.now);
        if w.orphaned > 0 {
            did.push(format!("Cleared {} run{} that were stuck with nothing driving them; they are marked interrupted and can be resumed.", w.orphaned, if w.orphaned == 1 { "" } else { "s" }));
        }
    }
    let mut lines = Vec::new();
    let mut proposals = Vec::new();
    let mut needs_you = Vec::new();
    for (id, name, judged) in all_judged(ctx, flows) {
        let h = health_of(&name, id, &judged);
        for s in ctx.sched.list(ctx.owner, Some(id)).unwrap_or_default() {
            if !s.enabled && s.last_status.contains("repeated failures") {
                needs_you.push(format!("The schedule \"{}\" was switched off after repeated failures. I will not switch it back on until the cause is fixed.", s.name));
            }
        }
        if matches!(h.state, HealthState::Healthy | HealthState::Idle) {
            continue;
        }
        lines.push(format!("• {name}: {} — {}", state_word(h.state), h.reasons.join(" ")));
        if let Some(j) = judged.iter().find(|j| j.verdict.kind == VerdictKind::Failed).filter(|_| judged.first().is_some_and(|f| f.verdict.kind == VerdictKind::Failed)) {
            let (p, guide, _) = proposals_for(ctx, &name, j);
            if let Some(g) = guide {
                needs_you.push(format!("{name}: {g}"));
            }
            proposals.extend(p);
        }
    }
    let mut text = String::new();
    if lines.is_empty() && did.is_empty() && needs_you.is_empty() {
        text.push_str("I checked every workflow: they are all healthy or have not run yet. Nothing needs fixing.");
    } else {
        if lines.is_empty() {
            text.push_str("Every workflow that has run is healthy.");
        } else {
            text.push_str(&format!("Workflows with problems:\n{}", lines.join("\n")));
        }
        if !did.is_empty() {
            text.push_str(&format!("\n\nWhat I did:\n{}", did.iter().map(|d| format!("• {d}")).collect::<Vec<_>>().join("\n")));
        }
        if !proposals.is_empty() {
            text.push_str("\n\nChanges I can make if you approve (none are made until then):");
            for p in &proposals {
                text.push_str(&format!("\n• {}: {}", p.flow_name, p.title));
            }
        }
        if !needs_you.is_empty() {
            text.push_str(&format!("\n\nOnly you can do these:\n{}", needs_you.iter().map(|d| format!("• {d}")).collect::<Vec<_>>().join("\n")));
        }
    }
    Reply { text, proposals, did }
}

/// For tests and the dashboard: which class a failed run's step had.
pub fn class_of(error: &str) -> ErrorClass {
    super::classify(error)
}
