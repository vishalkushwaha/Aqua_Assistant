//! Proactive watch (card R7-19): checks that run by themselves on a timer and turn what they find into gaps, and where a gap has a rule, into proposals. Never into an action.
//!
//! * **Off unless `AQUA_SELF_REPAIR_WATCH=on`** (also `1`, `true`, `yes`), read when Aqua starts. The interval is `AQUA_SELF_REPAIR_WATCH_SECS`: 600 by default, 60 at least, a day at most
//!   ([`interval_secs`]). The first pass comes half a minute after start, so the rest of the program is up.
//! * **A pass only reads.** It reads the saved flows, their recent runs, the schedules and the tool host's list of tools that cannot run, and it writes to one place: the self-repair
//!   record (a gap, and a proposal that waits). It starts no flow, runs no tool, allows no folder, edits no flow and calls no model. Approving is still the person's click.
//! * **What a pass notices** ([`pass`]): a flow whose health score is below [`HEALTH_SCORE_FLOOR`] or that failed [`STREAK_LIMIT`] runs in a row (`flow_unhealthy`, one gap for each flow); a
//!   schedule whose last time failed, did not start or was missed (`schedule_failed`, one for each schedule); a tool the host lists as unavailable that a saved flow uses
//!   (`tool_unavailable`, one for each tool); a saved flow with a step that posts to a platform that is not built (`platform_not_built`, the gap a refused run writes, found before any run).
//!   The count of a gap grows with every pass that still sees the problem. Only the last kind has a rule that makes proposals; it makes them once for a gap, so a proposal that was
//!   rejected does not come back by itself.
//! * A recipe that failed its last replay is not watched: the recipe store keeps no such record.
//! * A pass is a pure function of a snapshot ([`WatchInput`]) that a [`WatchSource`] builds with reads only, so the tests need no timer. The thread builds the snapshot and sleeps; a pass
//!   that panics or runs longer than [`PASS_LIMIT`] is cut and logged, and the thread holds no lock while it sleeps.

use super::gaps::not_built_gap;
use super::proposals::{create_proposals_for, Env, HostFolders};
use super::store::{Gap, GapKind, NewGap, RepairStore};
use crate::flows::model::{FlowDef, NodeKind};
use crate::flows::platform;
use crate::flows::run::store::RunStore;
use crate::flows::sched::store::SchedStore;
use crate::flows::store::FlowStore;
use crate::flows::supervisor::health::{health_of, judged_runs, Health, WINDOW};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// The switch.
pub const SWITCH: &str = "AQUA_SELF_REPAIR_WATCH";
/// The interval, in seconds.
pub const SECS_VAR: &str = "AQUA_SELF_REPAIR_WATCH_SECS";
pub const DEFAULT_SECS: u64 = 600;
pub const MIN_SECS: u64 = 60;
pub const MAX_SECS: u64 = 86_400;
/// A flow with a health score below this (0 to 100) becomes a gap. It is the line below which the flow's own health page calls it failing.
pub const HEALTH_SCORE_FLOOR: u32 = 60;
/// A flow whose latest runs failed this many times in a row becomes a gap, whatever its score.
pub const STREAK_LIMIT: usize = 2;
/// A pass that takes longer than this is cut.
pub const PASS_LIMIT: Duration = Duration::from_secs(10);
/// The most flows one pass looks at.
const MAX_FLOWS: usize = 300;
const FIRST_PASS_DELAY: Duration = Duration::from_secs(30);

/// Is the watch switched on? Anything but `on`, `1`, `true` or `yes` is off.
pub fn enabled() -> bool {
    crate::research::switch_is_on(std::env::var(SWITCH).ok().as_deref())
}

/// The interval from the text of the variable: nothing, or text that is not a whole number, is [`DEFAULT_SECS`]; a number below [`MIN_SECS`] is raised to it and one above
/// [`MAX_SECS`] (a day) is lowered to it.
pub fn interval_secs(value: Option<&str>) -> u64 {
    match value.map(str::trim).and_then(|v| v.parse::<u64>().ok()) {
        Some(n) => n.clamp(MIN_SECS, MAX_SECS),
        None => DEFAULT_SECS,
    }
}

// ---- what a pass looks at ----------------------------------------------------------------------------------

/// How a flow's recent runs went (what `supervisor::health` works out from the run history).
#[derive(Debug, Clone, PartialEq)]
pub struct HealthFacts {
    /// 0 to 100; `None` when no real run has finished yet, so there is nothing to judge.
    pub score: Option<u32>,
    pub runs: usize,
    pub failed: usize,
    /// Failures in a row, newest first.
    pub failure_streak: usize,
    /// The most common kind of failure lately and how many.
    pub top_problem: Option<(String, usize)>,
    pub reasons: Vec<String>,
}

pub fn facts_from_health(h: &Health) -> HealthFacts {
    HealthFacts { score: h.score, runs: h.runs, failed: h.failed, failure_streak: h.failure_streak, top_problem: h.top_problem.clone(), reasons: h.reasons.clone() }
}

#[derive(Debug, Clone)]
pub struct FlowFacts {
    pub flow_id: i64,
    pub name: String,
    pub def: FlowDef,
    /// `None` when the runs could not be read.
    pub health: Option<HealthFacts>,
}

/// A schedule and how its last time went.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleFacts {
    pub id: i64,
    pub flow_id: i64,
    pub name: String,
    pub flow_name: String,
    pub enabled: bool,
    /// The word the scheduler keeps on the schedule (`succeeded`, `failed`, `could not start: ...`, `missed (asking you)`, ...).
    pub last_status: String,
    /// What was done about the latest time that came due: `started`, `skipped`, `missed_pending`, `failed_to_start`, ... (empty when none came).
    pub last_outcome: String,
    pub last_reason: String,
}

/// Everything one pass looks at, taken at one moment.
#[derive(Debug, Clone)]
pub struct WatchInput {
    pub now: i64,
    pub flows: Vec<FlowFacts>,
    pub schedules: Vec<ScheduleFacts>,
    /// Tools that exist but cannot run on this machine right now, and why (`GET /api/v1/tools/status` lists the same).
    pub unavailable_tools: Vec<(String, String)>,
}

/// Where a pass gets its snapshot. It has reads and nothing else, so a source cannot change anything.
pub trait WatchSource: Send + Sync {
    fn snapshot(&self, now: i64) -> WatchInput;
}

/// The stores of the running program.
pub struct LiveSource {
    pub flows: Arc<FlowStore>,
    pub runs: Arc<RunStore>,
    pub sched: Arc<SchedStore>,
    pub tools: Option<Arc<aqua_tool_host::ToolHost>>,
    pub owner: String,
}

impl LiveSource {
    pub fn process_wide() -> Self {
        Self {
            flows: crate::flows::services::global().flows(),
            runs: crate::flows::services::global().runs(),
            sched: crate::flows::services::global().schedules(),
            tools: crate::agent_tools::host().cloned(),
            owner: crate::flows::model::current_owner().to_string(),
        }
    }
}

impl WatchSource for LiveSource {
    fn snapshot(&self, now: i64) -> WatchInput {
        let mut flows = Vec::new();
        for summary in self.flows.list(&self.owner).unwrap_or_default().into_iter().take(MAX_FLOWS) {
            let Ok(Some(flow)) = self.flows.get(&self.owner, summary.id) else { continue };
            let name = flow.def.name.clone();
            let health = judged_runs(&self.runs, &self.owner, flow.id, WINDOW).ok().map(|judged| facts_from_health(&health_of(&name, flow.id, &judged)));
            flows.push(FlowFacts { flow_id: flow.id, name, def: flow.def, health });
        }
        let names: BTreeMap<i64, String> = flows.iter().map(|f| (f.flow_id, f.name.clone())).collect();
        let mut schedules = Vec::new();
        for s in self.sched.list(&self.owner, None).unwrap_or_default() {
            let last = self.sched.occurrences(&self.owner, s.id, 1).ok().and_then(|o| o.into_iter().next());
            schedules.push(ScheduleFacts {
                id: s.id,
                flow_id: s.flow_id,
                name: s.name.clone(),
                flow_name: names.get(&s.flow_id).cloned().unwrap_or_default(),
                enabled: s.enabled,
                last_status: s.last_status.clone(),
                last_outcome: last.as_ref().map(|o| o.outcome.clone()).unwrap_or_default(),
                last_reason: last.map(|o| o.reason).unwrap_or_default(),
            });
        }
        let unavailable_tools: Vec<(String, String)> = match &self.tools {
            Some(host) => host.catalog().into_iter().filter_map(|(m, why)| why.map(|w| (m.name, w))).collect(),
            None => Vec::new(),
        };
        WatchInput { now, flows, schedules, unavailable_tools }
    }
}

// ---- the checks --------------------------------------------------------------------------------------------

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}...", flat.chars().take(max).collect::<String>().trim_end())
    }
}

/// A flow whose score is below the floor or whose latest runs failed in a row. The key is the flow, so the same flow is one row whose count grows.
fn flow_gap(f: &FlowFacts) -> Option<NewGap> {
    let h = f.health.as_ref()?;
    let low = h.score.is_some_and(|s| s < HEALTH_SCORE_FLOOR);
    let streak = h.failure_streak >= STREAK_LIMIT;
    if !low && !streak {
        return None;
    }
    let name = one_line(&f.name, 80);
    let why = if streak { format!("its last {} runs in a row failed", h.failure_streak) } else { format!("its health score is {} of 100 over its last {} runs", h.score.unwrap_or(0), h.runs) };
    let mut g = NewGap::new(GapKind::FlowUnhealthy, f.flow_id.to_string(), format!("The flow \"{name}\" is not working: {why}"));
    g.detail = h.reasons.join(" ");
    g.source = "watch".into();
    g.flow_id = Some(f.flow_id);
    g.data = json!({
        "flow_id": f.flow_id,
        "flow_name": name,
        "score": h.score,
        "runs": h.runs,
        "failed": h.failed,
        "failure_streak": h.failure_streak,
        "top_problem": h.top_problem.as_ref().map(|(class, count)| json!({ "class": class, "count": count })),
    });
    Some(g)
}

/// What is wrong with a schedule, in words, and the scheduler's own reason: its last time failed, did not start, or was missed. A time that was skipped because the previous run was
/// still going, or because the machine was not ready, is not a failure.
pub fn schedule_problem(s: &ScheduleFacts) -> Option<(&'static str, String)> {
    let status = s.last_status.trim();
    let reason = s.last_reason.trim();
    let said = if reason.is_empty() { status } else { reason };
    if s.last_outcome == "failed_to_start" || status.starts_with("could not start") {
        return Some(("could not start its last run", said.to_string()));
    }
    if status.starts_with("failed") {
        return Some(("had a last run that failed", status.to_string()));
    }
    if s.last_outcome == "missed_pending" || status.starts_with("missed") || (s.last_outcome == "skipped" && reason.starts_with("missed")) {
        return Some(("missed a scheduled time", said.to_string()));
    }
    None
}

fn schedule_gap(s: &ScheduleFacts) -> Option<NewGap> {
    let (what, why) = schedule_problem(s)?;
    let flow_name = if s.flow_name.is_empty() { "(deleted flow)".to_string() } else { one_line(&s.flow_name, 80) };
    let mut g = NewGap::new(GapKind::ScheduleFailed, s.id.to_string(), format!("The schedule \"{}\" of the flow \"{flow_name}\" {what}", one_line(&s.name, 80)));
    g.detail = why;
    g.source = "watch".into();
    g.flow_id = Some(s.flow_id);
    g.data = json!({ "schedule_id": s.id, "flow_id": s.flow_id, "flow_name": flow_name, "enabled": s.enabled, "last_status": one_line(&s.last_status, 160), "last_outcome": s.last_outcome });
    Some(g)
}

/// The tools the enabled steps of a flow use, by the name the tool host knows them by. A name that another step fills in (`{{...}}`) is not known ahead of time and is left out.
pub fn tools_used(def: &FlowDef) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for n in def.nodes.iter().filter(|n| n.enabled) {
        let setting = |key: &str| n.config.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        match n.kind {
            NodeKind::Tool => {
                let tool = setting("tool");
                if !tool.is_empty() && !tool.contains("{{") {
                    out.insert(tool);
                }
            }
            NodeKind::File => match setting("operation").as_str() {
                "read" => {
                    out.insert("fs.read".to_string());
                }
                "write" => {
                    out.insert("fs.write".to_string());
                }
                "list" => {
                    out.insert("fs.list".to_string());
                }
                _ => {}
            },
            NodeKind::Http => {
                out.insert("http.request".to_string());
            }
            _ => {}
        }
    }
    out
}

/// One gap for each unavailable tool that a saved flow uses.
fn tool_gaps(flows: &[FlowFacts], unavailable: &[(String, String)]) -> Vec<NewGap> {
    let uses: Vec<(&FlowFacts, BTreeSet<String>)> = flows.iter().map(|f| (f, tools_used(&f.def))).collect();
    let mut out = Vec::new();
    for (tool, why) in unavailable {
        let users: Vec<&FlowFacts> = uses.iter().filter(|(_, tools)| tools.contains(tool)).map(|(f, _)| *f).collect();
        if users.is_empty() {
            continue;
        }
        let n = users.len();
        let summary = format!("The tool {tool} cannot run ({}), and {n} saved flow{} use{} it", one_line(why, 120), if n == 1 { "" } else { "s" }, if n == 1 { "s" } else { "" });
        let mut g = NewGap::new(GapKind::ToolUnavailable, tool.clone(), summary);
        g.detail = format!("The tool host lists {tool} as unavailable: {}", one_line(why, 200));
        g.source = "watch".into();
        g.flow_id = if n == 1 { Some(users[0].flow_id) } else { None };
        g.data = json!({
            "tool": tool,
            "why": one_line(why, 200),
            "flow_count": n,
            "flows": users.iter().take(5).map(|f| json!({ "flow_id": f.flow_id, "name": one_line(&f.name, 60) })).collect::<Vec<_>>(),
        });
        out.push(g);
    }
    out
}

/// A gap for each enabled step of a flow that posts to a platform that is not built.
fn platform_gaps(f: &FlowFacts) -> Vec<NewGap> {
    f.def
        .nodes
        .iter()
        .filter(|n| n.enabled && n.kind == NodeKind::SocialMedia)
        .filter_map(|n| {
            let p = n.config.get("platform").and_then(|v| v.as_str()).and_then(platform::get)?;
            let why = platform::not_built_note(p)?;
            Some(not_built_gap(f.flow_id, &f.name, n, p, &why))
        })
        .collect()
}

// ---- one pass ----------------------------------------------------------------------------------------------

/// What one pass did.
#[derive(Debug, Default, PartialEq)]
pub struct PassReport {
    pub flows_unhealthy: usize,
    pub schedules_failed: usize,
    pub tools_unavailable: usize,
    pub platforms_not_built: usize,
    /// The ids of the gaps the pass wrote (a gap seen again is listed too).
    pub gaps: Vec<i64>,
    /// Proposals made in this pass; they wait for the person.
    pub proposals_made: usize,
}

fn note(store: &RepairStore, g: NewGap, at: i64) -> Option<Gap> {
    match store.record(g, at) {
        Ok(gap) => Some(gap),
        Err(e) => {
            tracing::debug!("self repair watch: a gap was not recorded: {e}");
            None
        }
    }
}

/// A gap gets proposals from the watch once: when it is new and has never had one. A proposal that was rejected, applied or undone is the person's answer, and the watch does not ask again.
fn should_propose(store: &RepairStore, gap: &Gap) -> bool {
    gap.status == "new" && store.proposals(None, Some(gap.id), 1).map(|p| p.is_empty()).unwrap_or(false)
}

/// One pass over a snapshot: record what is wrong, and make the proposals that the gaps' rules give. Nothing is applied, started or changed.
pub fn pass(input: &WatchInput, store: &RepairStore, env: &Env<'_>) -> PassReport {
    let mut report = PassReport::default();
    for f in &input.flows {
        if let Some(gap) = flow_gap(f).and_then(|g| note(store, g, input.now)) {
            report.flows_unhealthy += 1;
            report.gaps.push(gap.id);
        }
        for g in platform_gaps(f) {
            let Some(gap) = note(store, g, input.now) else { continue };
            report.platforms_not_built += 1;
            report.gaps.push(gap.id);
            if should_propose(store, &gap) {
                match create_proposals_for(store, env, gap.id) {
                    Ok(made) => report.proposals_made += made.proposals.len(),
                    Err(e) => tracing::debug!("self repair watch: no proposals for gap {}: {e}", gap.id),
                }
            }
        }
    }
    for s in &input.schedules {
        if let Some(gap) = schedule_gap(s).and_then(|g| note(store, g, input.now)) {
            report.schedules_failed += 1;
            report.gaps.push(gap.id);
        }
    }
    for g in tool_gaps(&input.flows, &input.unavailable_tools) {
        if let Some(gap) = note(store, g, input.now) {
            report.tools_unavailable += 1;
            report.gaps.push(gap.id);
        }
    }
    report
}

/// A snapshot from `source`, then a pass over it. The source can only read.
pub fn run_pass(source: &dyn WatchSource, store: &RepairStore, env: &Env<'_>) -> PassReport {
    let input = source.snapshot(env.now);
    pass(&input, store, env)
}

// ---- the thread --------------------------------------------------------------------------------------------

/// Why a pass did not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    Panicked,
    TimedOut,
    NotStarted,
}

/// Run `work` on a thread of its own and wait at most `limit` for it. A panic is caught. A pass that runs over is given up on, not killed (a thread cannot be): it ends on its own, and the
/// caller does not start another until it has (see `watch_loop`).
pub fn run_guarded<T: Send + 'static>(limit: Duration, work: impl FnOnce() -> T + Send + 'static) -> Result<T, Cut> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("aqua-self-repair-pass".into()).spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        let _ = tx.send(outcome);
    });
    if spawned.is_err() {
        return Err(Cut::NotStarted);
    }
    match rx.recv_timeout(limit) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(Cut::Panicked),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(Cut::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(Cut::Panicked),
    }
}

/// Says "no pass is running" when it is dropped, also when the pass panicked or never started.
struct ResetOnDrop(Arc<AtomicBool>);

impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A pass over the stores of the running program.
fn live_pass(store: &RepairStore) -> PassReport {
    let now = chrono::Utc::now().timestamp();
    let source = LiveSource::process_wide();
    let env = Env { flows: &source.flows, folders: &HostFolders, owner: crate::flows::model::current_owner(), now };
    run_pass(&source, store, &env)
}

/// The loop of the watch job: a pass every `secs` seconds until the job is cancelled (the waits are the job's sleeps, which a cancel or the shutdown cuts short).
fn watch_loop(store: Arc<RepairStore>, secs: u64, job: &crate::jobs::JobCtx) {
    let running = Arc::new(AtomicBool::new(false));
    if !job.sleep(FIRST_PASS_DELAY) {
        return;
    }
    loop {
        if running.swap(true, Ordering::SeqCst) {
            tracing::warn!("self repair watch: the last pass is still running, so this one is skipped");
        } else {
            let reset = ResetOnDrop(running.clone());
            let st = store.clone();
            match run_guarded(PASS_LIMIT, move || {
                let _reset = reset;
                live_pass(&st)
            }) {
                Ok(r) if r.gaps.is_empty() => tracing::debug!("self repair watch: a pass found nothing"),
                Ok(r) => tracing::info!("self repair watch: a pass noticed {} problem(s) and made {} proposal(s)", r.gaps.len(), r.proposals_made),
                Err(Cut::Panicked) => tracing::error!("self repair watch: a pass panicked and was cut"),
                Err(Cut::TimedOut) => tracing::warn!("self repair watch: a pass took longer than {} seconds and was cut; the next one waits for it to end", PASS_LIMIT.as_secs()),
                Err(Cut::NotStarted) => tracing::warn!("self repair watch: a pass could not be started"),
            }
        }
        if !job.sleep(Duration::from_secs(secs)) {
            return;
        }
    }
}

static STARTED: AtomicBool = AtomicBool::new(false);

/// Start the watch thread if `AQUA_SELF_REPAIR_WATCH` is on. True when this call started it; a second call starts nothing.
pub fn start_if_enabled(store: &Arc<RepairStore>) -> bool {
    if !enabled() {
        return false;
    }
    let secs = interval_secs(std::env::var(SECS_VAR).ok().as_deref());
    if STARTED.swap(true, Ordering::SeqCst) {
        return false;
    }
    let store = store.clone();
    // A job of the registry (`jobs`): it promises a sign of life every interval and the longest a pass may take.
    let promise = Duration::from_secs(secs) + PASS_LIMIT;
    match crate::jobs::spawn(crate::jobs::JobKind::Watcher, "aqua-self-repair-watch", Some(promise), move |job| {
        watch_loop(store, secs, &job);
        Ok(())
    }) {
        Ok(_) => {
            tracing::info!("self repair watch: on, a pass every {secs} seconds");
            true
        }
        Err(e) => {
            tracing::warn!("self repair watch: the thread could not be started: {e}");
            STARTED.store(false, Ordering::SeqCst);
            false
        }
    }
}
