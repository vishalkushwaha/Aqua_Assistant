//! Health, history and errors, computed from what actually happened: the verdict of each finished run.
//! Nothing is stored separately, so it can never disagree with the run history.

use super::classify::ErrorClass;
use super::verdict::{judge, Verdict, VerdictKind};
use crate::flows::run::model::Run;
use crate::flows::run::store::RunStore;
use serde::Serialize;
use std::collections::BTreeMap;

pub type R<T> = Result<T, String>;

/// How many of a flow's latest finished runs are looked at.
pub const WINDOW: u32 = 20;
const DAY: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// Nothing has run yet, or only dry runs: there is nothing to judge.
    Idle,
    Healthy,
    /// Mostly works, but recent trouble.
    Degraded,
    /// Recent runs keep failing.
    Failing,
}

#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub flow_id: i64,
    pub name: String,
    pub state: HealthState,
    /// 0 to 100, from the recent runs; `None` when there is nothing to judge.
    pub score: Option<u32>,
    pub runs: usize,
    pub succeeded: usize,
    pub recovered: usize,
    pub failed: usize,
    pub unverified: usize,
    pub last_run_at: i64,
    pub last_success_at: i64,
    /// Failures in a row, newest first, ending at the first run that did not fail.
    pub failure_streak: usize,
    /// The most common kind of failure lately, and how many.
    pub top_problem: Option<(String, usize)>,
    /// In words: why it is in this state.
    pub reasons: Vec<String>,
}

/// One finished run and how it ended.
pub struct Judged {
    pub run: Run,
    pub verdict: Verdict,
    /// The step that failed, if one did.
    pub failed_step: Option<String>,
}

/// The last `limit` finished, non-dry runs of a flow, newest first, each judged.
pub fn judged_runs(runs: &RunStore, owner: &str, flow_id: i64, limit: u32) -> R<Vec<Judged>> {
    let mut out = Vec::new();
    for r in runs.list_runs(owner, Some(flow_id), limit * 2)?.into_iter().filter(|r| r.status.is_finished() && !r.dry_run) {
        if out.len() >= limit as usize {
            break;
        }
        let Some(full) = runs.get_run(owner, r.id)? else { continue };
        let nodes = runs.node_runs(r.id)?;
        let verdict = judge(&full, &nodes, &full.def);
        let failed_step = nodes.iter().find(|n| matches!(n.status, crate::flows::run::model::NodeStatus::Failed | crate::flows::run::model::NodeStatus::NeedsAction)).map(|n| full.def.node(&n.node_id).map(|x| x.name.clone()).unwrap_or_else(|| n.node_id.clone()));
        out.push(Judged { run: full, verdict, failed_step });
    }
    Ok(out)
}

pub fn health_of(name: &str, flow_id: i64, judged: &[Judged]) -> Health {
    let count = |k: VerdictKind| judged.iter().filter(|j| j.verdict.kind == k).count();
    let (succeeded, recovered, failed, unverified) = (count(VerdictKind::Success), count(VerdictKind::Recovered), count(VerdictKind::Failed), count(VerdictKind::Unknown));
    let runs = judged.len();
    let failure_streak = judged.iter().take_while(|j| j.verdict.kind == VerdictKind::Failed).count();
    let last_run_at = judged.first().map(|j| j.run.finished_at).unwrap_or(0);
    let last_success_at = judged.iter().find(|j| matches!(j.verdict.kind, VerdictKind::Success | VerdictKind::Recovered)).map(|j| j.run.finished_at).unwrap_or(0);
    let mut kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
    for j in judged.iter().filter(|j| j.verdict.kind == VerdictKind::Failed) {
        *kinds.entry(j.verdict.class.unwrap_or(ErrorClass::Unknown).label()).or_default() += 1;
    }
    let top_problem = kinds.into_iter().max_by_key(|(_, n)| *n).map(|(k, n)| (k.to_string(), n));

    // Score: a clean success counts fully, a recovery a little less, "could not confirm" half, a failure nothing.
    // Newer runs weigh more, so a flow that was broken and is fixed climbs back.
    let (mut got, mut total) = (0.0, 0.0);
    for (i, j) in judged.iter().enumerate() {
        let w = 1.0 + (runs - i) as f64 / runs as f64;
        total += w;
        got += w * match j.verdict.kind {
            VerdictKind::Success => 1.0,
            VerdictKind::Recovered => 0.85,
            VerdictKind::Unknown => 0.5,
            _ => 0.0,
        };
    }
    let score = (runs > 0).then(|| ((got / total) * 100.0).round() as u32);
    let state = match (score, failure_streak) {
        (None, _) => HealthState::Idle,
        (_, s) if s >= 2 => HealthState::Failing,
        (Some(sc), _) if sc >= 90 && failed == 0 && unverified == 0 => HealthState::Healthy,
        (Some(sc), _) if sc >= 90 => HealthState::Healthy,
        (Some(sc), _) if sc >= 60 => HealthState::Degraded,
        _ => HealthState::Failing,
    };
    let mut reasons = Vec::new();
    match state {
        HealthState::Idle => reasons.push("No real run has finished yet, so there is nothing to judge.".to_string()),
        _ => {
            reasons.push(format!("{succeeded} of the last {runs} runs worked cleanly{}.", if recovered > 0 { format!(", {recovered} after recovering") } else { String::new() }));
            if failure_streak >= 2 {
                reasons.push(format!("The last {failure_streak} runs in a row failed."));
            }
            if failed > 0 {
                if let Some((k, n)) = &top_problem {
                    reasons.push(format!("Most common problem: {k} ({n} time{}).", if *n == 1 { "" } else { "s" }));
                }
            }
            if unverified > 0 {
                reasons.push(format!("{unverified} run{} finished without proof that the work was done.", if unverified == 1 { "" } else { "s" }));
            }
        }
    }
    Health { flow_id, name: name.into(), state, score, runs, succeeded, recovered, failed, unverified, last_run_at, last_success_at, failure_streak, top_problem, reasons }
}

// ---- reliability over time -----------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct DayBucket {
    /// Days since the start of the window's first day (0 = oldest).
    pub day: i64,
    /// Unix time of that day's start (UTC).
    pub starts_at: i64,
    pub success: usize,
    pub recovered: usize,
    pub failed: usize,
    pub unverified: usize,
}

/// One bucket per day for the last `days` days ending at `now`, oldest first.
pub fn history(judged: &[Judged], now: i64, days: i64) -> Vec<DayBucket> {
    let today = now - now.rem_euclid(DAY);
    let mut out: Vec<DayBucket> = (0..days).map(|i| DayBucket { day: i, starts_at: today - (days - 1 - i) * DAY, ..Default::default() }).collect();
    for j in judged {
        let at = if j.run.finished_at > 0 { j.run.finished_at } else { j.run.created_at };
        let idx = days - 1 - (today - (at - at.rem_euclid(DAY))) / DAY;
        if !(0..days).contains(&idx) {
            continue;
        }
        let b = &mut out[idx as usize];
        match j.verdict.kind {
            VerdictKind::Success => b.success += 1,
            VerdictKind::Recovered => b.recovered += 1,
            VerdictKind::Failed => b.failed += 1,
            VerdictKind::Unknown => b.unverified += 1,
            VerdictKind::Waiting => {}
        }
    }
    out
}

// ---- the error dashboard --------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ErrorGroup {
    pub class: &'static str,
    pub label: &'static str,
    pub explain: &'static str,
    pub count: usize,
    pub flows: Vec<String>,
    pub last_at: i64,
    /// The most recent message of this kind (already redacted in the run record).
    pub example: String,
    pub last_run_id: i64,
    pub last_flow_id: i64,
}

/// Failures grouped by kind, most frequent first.
pub fn error_groups(all: &[(String, Vec<Judged>)], since: i64) -> Vec<ErrorGroup> {
    let mut groups: BTreeMap<&'static str, ErrorGroup> = BTreeMap::new();
    for (name, judged) in all {
        for j in judged.iter().filter(|j| j.verdict.kind == VerdictKind::Failed && j.run.finished_at >= since) {
            let c = j.verdict.class.unwrap_or(ErrorClass::Unknown);
            let g = groups.entry(c.key()).or_insert_with(|| ErrorGroup { class: c.key(), label: c.label(), explain: c.explain(), count: 0, flows: vec![], last_at: 0, example: String::new(), last_run_id: 0, last_flow_id: 0 });
            g.count += 1;
            if !g.flows.contains(name) {
                g.flows.push(name.clone());
            }
            if j.run.finished_at >= g.last_at {
                g.last_at = j.run.finished_at;
                g.example = j.verdict.reason.chars().take(240).collect();
                g.last_run_id = j.run.id;
                g.last_flow_id = j.run.flow_id;
            }
        }
    }
    let mut v: Vec<ErrorGroup> = groups.into_values().collect();
    v.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_at.cmp(&a.last_at)));
    v
}
