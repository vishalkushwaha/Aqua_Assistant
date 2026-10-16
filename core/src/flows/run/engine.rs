//! Runs a flow: decides which steps can go, runs them (independent ones at the same time), keeps the
//! state in the database after every change, and stops cleanly whenever it has to wait for a person.
//!
//! How a step becomes ready: every connection into it has been *decided* (the step before it finished,
//! and either took that exit or did not), and at least one carries the run in. A step whose connections
//! all say "this path was not taken" is skipped, and that spreads. So a condition's untaken side, and
//! everything only reachable through it, is skipped, and a step where two branches join runs once, when
//! whichever branch was taken arrives.
//!
//! A run is never a thread's memory. When nothing can go on until someone answers or a time comes, the
//! driver returns and holds nothing; answering (or the clock) starts a driver again from the stored state.

use super::exec::{Executors, NodeCtx, NodeError, NodeResult};
use super::model::*;
use super::store::RunStore;
use crate::flows::looping;
use crate::flows::model::*;
use crate::flows::registry;
use crate::workflows::schedule::CronExpr;
use crate::workflows::template::{self, Target};
use crate::flows::zone::Zone;
use chrono::NaiveDateTime;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

// The For each step: beginning an item, ending one, and what a run does with a loop that was left partway (card R7-12).
mod loops;

pub type R<T> = Result<T, String>;

/// Waits `secs` seconds; false if the run was cancelled meanwhile.
pub type Sleeper = dyn Fn(u64, &AtomicBool) -> bool + Send + Sync;

pub fn real_sleep(secs: u64, cancel: &AtomicBool) -> bool {
    for _ in 0..secs.saturating_mul(4) {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !cancel.load(Ordering::SeqCst)
}

pub const MAX_PARALLEL: usize = 4;
const MAX_BACKOFF_SECS: u64 = 300;

#[derive(Default)]
pub struct Control {
    pub cancel: AtomicBool,
    pub pause: AtomicBool,
}

pub struct Engine {
    pub store: Arc<RunStore>,
    pub execs: Arc<Executors>,
    pub sleep: Arc<Sleeper>,
    /// Unix seconds. Real time in Aqua; tests move it to test waits without waiting.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn first_port(kind: NodeKind) -> String {
    default_exit(kind)
}

/// The exit a step leaves by when nothing says otherwise: it is switched off, it was skipped by the person, or it failed and the flow carries on. The first one, except for a
/// For each step: its first is "each", and a loop that did not run must not run what is inside it once without an item, so it goes on by "done".
pub fn default_exit(kind: NodeKind) -> String {
    if kind == NodeKind::Loop {
        return "done".to_string();
    }
    registry::spec(kind).out_ports[0].clone()
}

/// The exit a step leaves by when it failed and the flow carries on: its first one, and for a condition that could not be judged the "false" one (decision D-15 of
/// 2026-10-07): a question that has no answer is not a yes, so the flow never goes the way of "yes" because something broke.
fn exit_after_error(kind: NodeKind) -> String {
    if kind == NodeKind::Condition {
        "false".to_string()
    } else {
        first_port(kind)
    }
}

/// What a worker thread reports back.
enum Outcome {
    Done(NodeResult, u32),
    Failed(NodeError, u32),
    Cancelled,
}

fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.to_rfc3339()).unwrap_or_default()
}

/// The moment a Schedule step waits for.
fn schedule_moment(cfg: &Value, tz: &str, at: i64) -> Result<i64, String> {
    let zone = Zone::parse(tz)?;
    let repeat = cfg.get("repeat").and_then(|v| v.as_str()).unwrap_or("once");
    if repeat != "once" {
        let cron = cfg.get("cron").and_then(|v| v.as_str()).ok_or("a repeating schedule needs a cron expression")?;
        return zone.next_cron(&CronExpr::parse(cron)?, at).ok_or_else(|| "that schedule never comes".to_string());
    }
    let when = cfg.get("when").and_then(|v| v.as_str()).ok_or("the schedule has no date and time")?;
    let naive = NaiveDateTime::parse_from_str(when.trim(), "%Y-%m-%d %H:%M").map_err(|_| format!("\"{when}\" is not a date and time; write it like 2026-10-01 10:00"))?;
    zone.to_timestamp(naive).ok_or_else(|| format!("\"{when}\" does not exist in that time zone (clocks change)"))
}

/// A short, readable description of what a step is about to do, for the approval that asks first.
fn describe(node: &Node, cfg: &Value) -> String {
    let get = |k: &str| cfg.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut lines = vec![format!("Step: {}", node.name)];
    for (k, label) in [("platform", "Platform"), ("action", "Action"), ("target", "Element"), ("text", "Text"), ("media", "Media"), ("hashtags", "Hashtags"), ("url", "Address"), ("tool", "Tool")] {
        let v = get(k);
        if !v.is_empty() {
            lines.push(format!("{label}: {v}"));
        }
    }
    if lines.len() == 1 {
        lines.push(serde_json::to_string_pretty(cfg).unwrap_or_default().chars().take(1200).collect());
    }
    lines.join("\n")
}

struct State {
    run: Run,
    nodes: HashMap<String, NodeRun>,
    /// The first time a write of the run's state failed (a full disk, a locked or damaged database). A run whose state cannot be
    /// kept cannot be resumed or trusted, so it ends as failed instead of going on as if it were saved.
    save_error: Option<String>,
    /// For each For each step: the ids of the steps inside it.
    bodies: HashMap<String, Vec<String>>,
}

impl State {
    fn settled(&self, id: &str) -> bool {
        self.nodes.get(id).is_some_and(|n| n.status.is_settled())
    }
    /// Is the connection's start finished, or is it a For each step that is on an item (it has not finished, but its "each" exit is open for that item)?
    fn edge_decided(&self, e: &Edge) -> bool {
        self.settled(&e.from) || (e.port == "each" && self.iterating(&e.from))
    }
    fn edge_active(&self, e: &Edge) -> bool {
        (e.port == "each" && self.iterating(&e.from)) || self.nodes.get(&e.from).is_some_and(|n| n.status.is_settled() && n.taken.contains(&e.port))
    }
    fn incoming<'a>(&self, def: &'a FlowDef, id: &str) -> Vec<&'a Edge> {
        def.edges.iter().filter(|e| e.to == id && def.node(&e.from).is_some()).collect()
    }
    fn ready(&self, def: &FlowDef, node: &Node) -> bool {
        let Some(nr) = self.nodes.get(&node.id) else { return false };
        if nr.status != NodeStatus::Pending {
            return false;
        }
        let inc = self.incoming(def, &node.id);
        if inc.is_empty() {
            return node.kind == NodeKind::Trigger;
        }
        inc.iter().all(|e| self.edge_decided(e)) && inc.iter().any(|e| self.edge_active(e))
    }
    fn dead(&self, def: &FlowDef, node: &Node) -> bool {
        let Some(nr) = self.nodes.get(&node.id) else { return false };
        if nr.status != NodeStatus::Pending {
            return false;
        }
        let inc = self.incoming(def, &node.id);
        !inc.is_empty() && inc.iter().all(|e| self.edge_decided(e)) && !inc.iter().any(|e| self.edge_active(e))
    }
    /// What later steps can read: `{{nodes.<id>.<value>}}`. A For each step that is on an item shows that item, for the steps inside.
    fn context(&self, def: &FlowDef) -> Value {
        let mut nodes = Map::new();
        for (id, n) in &self.nodes {
            if n.status.is_settled() || self.iterating(id) {
                nodes.insert(id.clone(), Value::Object(n.outputs.clone()));
            }
        }
        let local = chrono::Local::now();
        // Which variables hold text from outside is the run's own record, kept among the variables; a template sees the variables without it, and the list beside them.
        let mut vars = self.run.vars.clone();
        let outside = match vars.remove(OUTSIDE_VARS) {
            Some(Value::Array(names)) => Value::Array(names),
            // A record that is there and is not a list is damaged, or was not written by the engine: nothing is known about any variable, so every one counts as outside.
            Some(_) => Value::Array(vars.keys().map(|k| Value::String(k.clone())).collect()),
            None => json!([]),
        };
        let mut ctx = json!({
            "nodes": nodes,
            "vars": vars,
            "trigger": { "kind": self.run.trigger_kind, "payload": self.run.payload },
            "now": { "date": local.format("%Y-%m-%d").to_string(), "time": local.format("%H:%M").to_string(), "iso": local.to_rfc3339() },
            "flow": { "id": self.run.flow_id, "name": def.name },
            "run": { "id": self.run.id },
        });
        ctx[template::OUTSIDE_VARS_KEY] = outside;
        ctx
    }
}

/// The step's settings with `{{ }}` filled in. Text that goes to a model is treated as an instruction
/// from the flow's author with outside values quoted or fenced as data, exactly as for workflows.
fn render_config(node: &Node, ctx: &Value) -> Result<Value, String> {
    let field = match node.kind {
        NodeKind::AiAgent => Some("goal"),
        NodeKind::Research => Some("topic"),
        NodeKind::TextGeneration | NodeKind::ImageGeneration => Some("prompt"),
        _ => None,
    };
    // A condition is read by the condition grammar with the values as values: filling the `{{ }}` in as text first would let text that came from a
    // page or a webhook become part of the condition. The same goes for the query of a Database step: a value reaches it only as a bound value (the step's "Values"), never
    // as text in the SQL (card R7-12).
    let raw_field = match node.kind {
        NodeKind::Condition => Some("expression"),
        NodeKind::Database => Some("query"),
        _ => None,
    };
    let mut cfg = node.config.clone();
    if let (Some(f), Some(obj)) = (field, cfg.as_object_mut()) {
        if let Some(Value::String(s)) = obj.get(f).cloned() {
            let mut r = template::render(&s, ctx, Target::AgentGoal)?;
            if template::uses_outside_data_in(&s, ctx) {
                r.push_str(template::DATA_NOTE);
            }
            obj.insert(f.to_string(), Value::String(r));
        }
    }
    let Some(obj) = cfg.as_object() else { return Ok(cfg) };
    let mut out = Map::new();
    for (k, v) in obj {
        if Some(k.as_str()) == field || Some(k.as_str()) == raw_field {
            out.insert(k.clone(), v.clone());
        } else {
            out.insert(k.clone(), template::render_json(v, ctx)?);
        }
    }
    Ok(Value::Object(out))
}

/// The longer wait of the "extra patience" fix.
const EXTRA_PATIENCE_SECS: u64 = 240;

/// What the supervisor's ladder says to do about a transient failure.
fn ladder_action(message: &str, failures: u32) -> Option<crate::flows::supervisor::Action> {
    use crate::flows::supervisor::{classify, decide, Situation};
    let class = classify(message);
    if !class.transient() {
        return None;
    }
    Some(decide(&Situation::first(class, failures)))
}

/// The log line for a step the ladder would have researched. Researching a failure in the background is not built (the rung is reached by no class that `ladder_action` lets
/// through), so the line says that the step failed and that nothing was started, never that a sub-agent was.
pub(crate) fn research_not_built(step: &str, message: &str) -> String {
    format!("\"{step}\" failed ({message}); researching a failure in the background is not built, so nothing was started and the step failed")
}

impl Engine {
    /// A line in the run's log. Losing one does not change what the run did, so it goes on, but the loss is in Aqua's own log.
    fn ev(&self, run_id: i64, node: Option<&str>, level: Level, kind: &str, msg: &str) {
        if let Err(e) = self.store.add_event(run_id, node, level, kind, msg, now()) {
            tracing::warn!("flow run {run_id}: a log line ({kind}) could not be saved: {e}");
        }
    }

    fn load(&self, owner: &str, run_id: i64) -> R<State> {
        let run = self.store.get_run(owner, run_id)?.ok_or_else(|| "no such run".to_string())?;
        let nodes = self.store.node_runs(run_id)?.into_iter().map(|n| (n.node_id.clone(), n)).collect();
        let bodies = looping::bodies(&run.def);
        Ok(State { run, nodes, save_error: None, bodies })
    }

    /// Steps that cannot run: no executor exists for their kind, or (a step that posts) the platform it names is one Aqua cannot post to yet. Empty means the flow can run.
    /// The second kind is found here, before the first step starts, so that nobody is asked to sign in and to approve a post that cannot be made (found on 2026-10-08: a
    /// Facebook flow went through the sign-in and the approval and only then said "not built yet").
    pub fn unsupported(&self, def: &FlowDef) -> Vec<String> {
        def.nodes
            .iter()
            .filter(|n| n.enabled)
            .filter_map(|n| {
                if !self.execs.supports(n.kind) {
                    return Some(format!("\"{}\" ({})", n.name, registry::spec(n.kind).label));
                }
                if n.kind == NodeKind::SocialMedia {
                    let why = n.config.get("platform").and_then(|v| v.as_str()).and_then(crate::flows::platform::get).and_then(crate::flows::platform::not_built_note)?;
                    return Some(format!("\"{}\" ({why})", n.name));
                }
                None
            })
            .collect()
    }

    fn needs_gate(&self, owner: &str, st: &State, node: &Node, nr: &NodeRun) -> bool {
        let def = &st.run.def;
        if st.run.dry_run || nr.gate_approved || !registry::high_impact(node) {
            return false;
        }
        match def.settings.approval {
            ApprovalPolicy::Never => return false,
            ApprovalPolicy::NewOnly => {
                if self.store.has_success_for_version(owner, st.run.flow_id, st.run.flow_version).unwrap_or(false) {
                    return false;
                }
            }
            ApprovalPolicy::Always => {}
        }
        // Covered by an explicit Approval step somewhere before it.
        !has_approval_before(def, &node.id)
    }

    fn save(&self, st: &mut State, n: NodeRun) {
        if let Err(e) = self.store.save_node(&n) {
            tracing::error!("flow run {}: the state of step '{}' could not be saved: {e}", st.run.id, n.node_id);
            st.save_error.get_or_insert(e);
        }
        st.nodes.insert(n.node_id.clone(), n);
    }

    fn finish_node(&self, st: &mut State, id: &str, f: impl FnOnce(&mut NodeRun)) {
        if let Some(mut n) = st.nodes.get(id).cloned() {
            f(&mut n);
            self.save(st, n);
        }
    }

    /// Run the flow until it finishes, has to wait, is paused, or is stopped.
    pub fn drive(&self, owner: &str, run_id: i64, control: &Control) -> R<RunStatus> {
        let mut st = self.load(owner, run_id)?;
        if st.run.status.is_finished() {
            return Ok(st.run.status);
        }
        let def = st.run.def.clone();
        let first = st.run.started_at == 0;
        self.store.set_status(run_id, RunStatus::Running, "", now())?;
        self.ev(run_id, None, Level::Info, if first { "run_started" } else { "run_resumed" }, &format!("{} \"{}\"{}", if first { "Started" } else { "Resumed" }, def.name, if st.run.dry_run { " (dry run: nothing outside Aqua is touched)" } else { "" }));

        let (tx, rx) = channel::<(String, Outcome)>();
        let mut running: HashSet<String> = HashSet::new();
        let mut failure: Option<String> = None;
        let mut final_status: Option<RunStatus> = None;

        std::thread::scope(|scope| -> R<()> {
            loop {
                // ---- settle: skip what can no longer be reached, wake what has waited long enough ----------
                loop {
                    let dead: Vec<String> = def.nodes.iter().filter(|n| st.dead(&def, n)).map(|n| n.id.clone()).collect();
                    if dead.is_empty() {
                        break;
                    }
                    for id in dead {
                        self.finish_node(&mut st, &id, |n| {
                            n.status = NodeStatus::Skipped;
                            n.taken = vec![];
                            n.finished_at = now();
                            n.note = "the path to it was not taken".into();
                        });
                        let name = def.node(&id).map(|n| n.name.clone()).unwrap_or_default();
                        self.ev(run_id, Some(&id), Level::Info, "node_skipped", &format!("Skipped \"{name}\": the path to it was not taken"));
                    }
                }
                let t = (self.clock)();
                let due: Vec<String> = st.nodes.values().filter(|n| n.status == NodeStatus::Waiting && matches!(n.wait, Some(WaitRequest::Until { ts }) if ts <= t)).map(|n| n.node_id.clone()).collect();
                for id in due {
                    let port = def.node(&id).map(|n| first_port(n.kind)).unwrap_or_else(|| "out".into());
                    self.finish_node(&mut st, &id, |n| {
                        n.status = NodeStatus::Succeeded;
                        n.taken = vec![port];
                        n.finished_at = t;
                        n.wait = None;
                    });
                    let name = def.node(&id).map(|n| n.name.clone()).unwrap_or_default();
                    self.ev(run_id, Some(&id), Level::Info, "node_succeeded", &format!("\"{name}\": the wait is over"));
                }

                // ---- the run's state could not be written: start nothing more, and end the run as failed ---------
                if let Some(e) = st.save_error.take() {
                    failure.get_or_insert(format!("state could not be saved: {e}"));
                }

                // ---- stop, pause -----------------------------------------------------------------------
                let cancelled = control.cancel.load(Ordering::SeqCst);
                let paused = control.pause.load(Ordering::SeqCst);

                // ---- launch what is ready ----------------------------------------------------------------
                let mut launched = false;
                if !cancelled && !paused && failure.is_none() {
                    // A For each step begins its next item, or ends, when the steps inside the one it is on are all done.
                    if self.advance_loops(&mut st, &def, &mut failure) {
                        launched = true;
                    }
                    if failure.is_none() {
                        let ready: Vec<Node> = def.nodes.iter().filter(|n| st.ready(&def, n)).cloned().collect();
                        for node in ready {
                            if running.len() >= MAX_PARALLEL {
                                break;
                            }
                            if self.launch(owner, &mut st, &def, &node, control, &tx, scope, &mut running, &mut failure)? {
                                launched = true;
                            }
                        }
                    }
                }

                // ---- collect what finished -----------------------------------------------------------------
                if !running.is_empty() {
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok((id, outcome)) => {
                            running.remove(&id);
                            self.handle_outcome(&mut st, &def, &id, outcome, &mut failure);
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => running.clear(),
                    }
                    continue;
                }
                if launched {
                    continue;
                }

                // ---- nothing running and nothing new to launch ---------------------------------------------------
                if cancelled {
                    final_status = Some(RunStatus::Cancelled);
                } else if paused && (def.nodes.iter().any(|n| st.ready(&def, n)) || st.loop_has_work()) {
                    final_status = Some(RunStatus::Paused);
                } else if let Some(f) = &failure {
                    self.park_loops(&mut st, "the run failed before it");
                    let ids: Vec<String> = st.nodes.values().filter(|n| n.status == NodeStatus::Pending).map(|n| n.node_id.clone()).collect();
                    for id in ids {
                        self.finish_node(&mut st, &id, |n| {
                            n.status = NodeStatus::Skipped;
                            n.taken = vec![];
                            n.note = "the run failed before it".into();
                        });
                    }
                    self.store.set_status(run_id, RunStatus::Failed, f, now())?;
                    final_status = Some(RunStatus::Failed);
                } else if st.nodes.values().any(|n| matches!(n.status, NodeStatus::Waiting | NodeStatus::NeedsAction)) {
                    final_status = Some(RunStatus::Waiting);
                } else if st.nodes.values().any(|n| n.status == NodeStatus::Interrupted) {
                    final_status = Some(RunStatus::Interrupted);
                } else if let Some(name) = st.stuck_loop(&def) {
                    // A For each step is on an item and nothing inside it can go on (it should not happen in a flow that was checked). Saying so is better than calling the run done.
                    failure = Some(format!("\"{name}\" could not go on: a step inside it never became ready"));
                    continue;
                } else {
                    final_status = Some(RunStatus::Succeeded);
                }
                return Ok(());
            }
        })?;

        let status = final_status.unwrap_or(RunStatus::Succeeded);
        match status {
            RunStatus::Failed => {} // already stored with its message
            RunStatus::Cancelled => {
                self.park_loops(&mut st, "the run was stopped");
                let ids: Vec<String> = st.nodes.values().filter(|n| n.status == NodeStatus::Pending).map(|n| n.node_id.clone()).collect();
                for id in ids {
                    self.finish_node(&mut st, &id, |n| {
                        n.status = NodeStatus::Skipped;
                        n.taken = vec![];
                        n.note = "the run was stopped".into();
                    });
                }
                // The event is written before the status: whoever sees the status finds the event (a reader that waits for "succeeded" and then reads the log did not).
                self.ev(run_id, None, Level::Info, "run_cancelled", "The run was stopped");
                self.store.set_status(run_id, status, "stopped", now())?;
            }
            RunStatus::Succeeded => {
                self.ev(run_id, None, Level::Info, "run_succeeded", "The run finished");
                self.store.set_status(run_id, status, "", now())?;
            }
            RunStatus::Waiting => {
                self.ev(run_id, None, Level::Info, "run_waiting", "Waiting for you, or for a time to come");
                self.store.set_status(run_id, status, "", now())?;
            }
            RunStatus::Paused => {
                self.ev(run_id, None, Level::Info, "run_paused", "Paused");
                self.store.set_status(run_id, status, "", now())?;
            }
            other => self.store.set_status(run_id, other, "", now())?,
        }
        if failure.is_some() {
            self.ev(run_id, None, Level::Error, "run_failed", failure.as_deref().unwrap_or(""));
        }
        // A run that is over keeps nothing with the executors (a run that waits for a person or for a time is not over).
        if matches!(status, RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled) {
            self.execs.run_ended(run_id);
        }
        Ok(status)
    }

    /// Start one ready step: logic steps are done here and now; the rest go to a worker thread.
    /// Returns whether something happened that changes what is ready.
    #[allow(clippy::too_many_arguments)]
    fn launch<'s>(
        &'s self,
        owner: &str,
        st: &mut State,
        def: &FlowDef,
        node: &Node,
        control: &'s Control,
        tx: &std::sync::mpsc::Sender<(String, Outcome)>,
        scope: &'s std::thread::Scope<'s, '_>,
        running: &mut HashSet<String>,
        failure: &mut Option<String>,
    ) -> R<bool> {
        let run_id = st.run.id;
        let mut nr = st.nodes.get(&node.id).cloned().ok_or("a step has no run record")?;
        let ctx = st.context(def);

        // A switched-off step is skipped and the flow carries on past it.
        if !node.enabled {
            nr.status = NodeStatus::Skipped;
            nr.taken = vec![first_port(node.kind)];
            nr.finished_at = now();
            nr.note = "switched off".into();
            self.save(st, nr);
            self.ev(run_id, Some(&node.id), Level::Info, "node_skipped", &format!("Skipped \"{}\": it is switched off", node.name));
            return Ok(true);
        }

        // A For each step that was left partway (the run was stopped by a crash, or a step inside it was tried again) goes on at its item, with the list it began with.
        if node.kind == NodeKind::Loop && nr.looping.is_some() {
            self.resume_loop(st, node);
            return Ok(true);
        }

        let cfg = match render_config(node, &ctx) {
            Ok(c) => c,
            Err(e) => {
                self.fail_node(st, node, &format!("could not fill in the settings: {e}"), 1, failure, false);
                return Ok(true);
            }
        };
        nr.status = NodeStatus::Running;
        nr.started_at = now();
        nr.attempts = nr.attempts.max(1);
        self.save(st, nr.clone());
        self.ev(run_id, Some(&node.id), Level::Info, "node_started", &format!("Started \"{}\"", node.name));

        let dry = st.run.dry_run;
        let get = |k: &str| cfg.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        // ---- the steps the engine runs itself -----------------------------------------------------
        match node.kind {
            NodeKind::Trigger => {
                let mut o = Map::new();
                o.insert("payload".into(), st.run.payload.clone());
                self.succeed(st, node, o, vec![first_port(node.kind)], "");
                return Ok(true);
            }
            NodeKind::SetVariable => {
                let name = get("name");
                let value = get("value");
                // The record of which variables hold text from outside is kept among the variables under a name that starts with two underscores, and a name that was
                // made from text from outside could name any variable (card R6-11): neither can be set by a step, or the record could be overwritten and every mark lost.
                let name_from_outside = node.config.get("name").and_then(|v| v.as_str()).is_some_and(|raw| template::uses_outside_data_in(raw, &ctx));
                if name.starts_with("__") || name_from_outside {
                    self.fail_node(st, node, "the name of a variable cannot start with two underscores (Aqua keeps its own record under such a name) and cannot be made from text that came from a page, a step or a webhook", 1, failure, false);
                    return Ok(true);
                }
                // A value made from text that came from outside (a step's output, a webhook, another such variable) is text from outside itself (card R6-11): it
                // goes on being fenced where it enters a model's prompt. Set from a plain value, a variable is the flow author's own again.
                let from_outside = node.config.get("value").and_then(|v| v.as_str()).is_some_and(|raw| template::uses_outside_data_in(raw, &ctx));
                let mut outside: Vec<Value> = st.run.vars.get(OUTSIDE_VARS).and_then(|v| v.as_array()).cloned().unwrap_or_default();
                outside.retain(|n| n.as_str() != Some(name.as_str()));
                if from_outside {
                    outside.push(Value::String(name.clone()));
                }
                if outside.is_empty() {
                    st.run.vars.remove(OUTSIDE_VARS);
                } else {
                    st.run.vars.insert(OUTSIDE_VARS.to_string(), Value::Array(outside));
                }
                st.run.vars.insert(name.clone(), Value::String(value.clone()));
                if let Err(e) = self.store.set_vars(run_id, &st.run.vars) {
                    tracing::error!("flow run {run_id}: the variables could not be saved: {e}");
                    st.save_error.get_or_insert(e);
                }
                let mut o = Map::new();
                o.insert("value".into(), Value::String(value));
                self.succeed(st, node, o, vec![first_port(node.kind)], &format!("{name} is set"));
                return Ok(true);
            }
            NodeKind::Loop => {
                self.start_loop(st, def, node, &cfg, failure);
                return Ok(true);
            }
            NodeKind::Condition => {
                // A lone {{path}} keeps the type it points at (true, 2, null), so read it by type: only strings
                // are text, and reading a number or a bool as text would make every such condition false.
                let expression = match cfg.get("expression") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Null) | None => String::new(),
                    Some(other) => other.to_string(),
                };
                let truth = match template::condition(&expression, &ctx) {
                    Ok(t) => t,
                    Err(e) => {
                        self.fail_node(st, node, &format!("could not evaluate the condition: {e}"), 1, failure, false);
                        return Ok(true);
                    }
                };
                let mut o = Map::new();
                o.insert("result".into(), Value::Bool(truth));
                self.succeed(st, node, o, vec![if truth { "true".into() } else { "false".into() }], &format!("the answer is {}", if truth { "yes" } else { "no" }));
                return Ok(true);
            }
            NodeKind::Notification => {
                self.ev(run_id, Some(&node.id), Level::Notify, "notify", &get("message"));
                // Card R6-14 (T6-14.7): the step can only show its message in the run's log. A flow that asked for email or Slack is told so, and does not look as if it was sent.
                let channel = get("channel");
                let note = if channel.is_empty() || channel == "in_app" {
                    String::new()
                } else {
                    let why = format!("\"{}\" was set to tell you by {channel}, which a flow step cannot send yet: the message is shown in this log only", node.name);
                    self.ev(run_id, Some(&node.id), Level::Warn, "notify_not_sent", &why);
                    format!("shown in the log only; not sent by {channel}")
                };
                self.succeed(st, node, Map::new(), vec![first_port(node.kind)], &note);
                return Ok(true);
            }
            NodeKind::Delay => {
                let secs = cfg.get("seconds").and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0).max(0.0) as i64;
                if dry || secs == 0 {
                    self.succeed(st, node, Map::new(), vec![first_port(node.kind)], if dry { "dry run: not waiting" } else { "" });
                } else {
                    self.wait(st, node, WaitRequest::Until { ts: (self.clock)() + secs }, &format!("Waiting {secs} seconds"));
                }
                return Ok(true);
            }
            NodeKind::Schedule => {
                match schedule_moment(&cfg, &def.settings.timezone, (self.clock)()) {
                    Ok(ts) => {
                        let mut o = Map::new();
                        o.insert("scheduled_for".into(), Value::String(iso(ts)));
                        if dry || ts <= (self.clock)() {
                            self.succeed(st, node, o, vec![first_port(node.kind)], if dry { "dry run: not waiting" } else { "the time has come" });
                        } else {
                            if let Some(n) = st.nodes.get_mut(&node.id) {
                                n.outputs = o.clone();
                            }
                            self.wait_with_outputs(st, node, WaitRequest::Until { ts }, o, &format!("Waiting until {}", iso(ts)));
                        }
                    }
                    Err(e) => self.fail_node(st, node, &e, 1, failure, false),
                }
                return Ok(true);
            }
            NodeKind::Approval => {
                let title = get("title");
                let preview = get("preview");
                if dry {
                    let mut o = Map::new();
                    o.insert("decision".into(), json!("approved"));
                    o.insert("edits".into(), json!(""));
                    o.insert("text".into(), json!(preview));
                    self.succeed(st, node, o, vec!["approved".into()], "dry run: approved automatically");
                } else {
                    let allow_edit = cfg.get("allow_edit").and_then(|v| v.as_bool()).unwrap_or(true);
                    self.wait(st, node, WaitRequest::Approval { title: if title.is_empty() { "Ready to go ahead".into() } else { title }, preview, allow_edit, implicit: false }, "Waiting for your approval");
                }
                return Ok(true);
            }
            NodeKind::UserInput => {
                let kind = if get("answer_kind").is_empty() { "text".to_string() } else { get("answer_kind") };
                if dry {
                    let mut o = Map::new();
                    o.insert("answer".into(), json!(if kind == "yes_no" { "yes" } else { "sample answer" }));
                    self.succeed(st, node, o, vec![first_port(node.kind)], "dry run: answered automatically");
                } else {
                    let options = get("options").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    self.wait(st, node, WaitRequest::Input { question: get("question"), answer_kind: kind, options }, "Waiting for your answer");
                }
                return Ok(true);
            }
            _ => {}
        }

        // ---- steps that reach outside the engine -----------------------------------------------------------
        // Which database a Database step asks is the flow author's choice: a name made from text that came from a page, a step or a webhook could point it at any database that is set up.
        if node.kind == NodeKind::Database && node.config.get("profile").and_then(|v| v.as_str()).is_some_and(|raw| template::uses_outside_data_in(raw, &ctx)) {
            self.fail_node(st, node, "the name of the database cannot be made from text that came from a page, a step or a webhook; write it in the step or give it as an input", 1, failure, false);
            return Ok(true);
        }
        if !dry && self.needs_gate(owner, st, node, &nr) {
            self.wait(st, node, WaitRequest::Approval { title: format!("Ready to run: {}", node.name), preview: describe(node, &cfg), allow_edit: false, implicit: true }, "Waiting for your approval before this step (it has an effect outside Aqua)");
            return Ok(true);
        }
        if dry {
            let mut o = Map::new();
            for out in &node.outputs {
                o.insert(out.clone(), json!(format!("[dry run] {} -> {out}", node.name)));
            }
            let risky = registry::high_impact(node);
            self.ev(run_id, Some(&node.id), if risky { Level::Warn } else { Level::Info }, "dry_run", &if risky { format!("Would run \"{}\" here. Nothing was sent.", node.name) } else { format!("Would run \"{}\" (simulated).", node.name) });
            let taken = vec![first_port(node.kind)];
            self.succeed(st, node, o, taken, "simulated");
            return Ok(true);
        }
        // A step inside a For each step has a ledger line for each item.
        let key = st.effect_key(&node.id);
        let risky = registry::ledgered(node);
        if risky {
            match self.store.effect_get(&key)? {
                Some((s, result)) if s == "done" => {
                    let outputs = result.as_object().cloned().unwrap_or_default();
                    self.succeed(st, node, outputs, vec![first_port(node.kind)], "already done earlier; not repeated");
                    self.ev(run_id, Some(&node.id), Level::Info, "not_repeated", &format!("\"{}\" had already been done, so it was not done again", node.name));
                    return Ok(true);
                }
                Some(_) => {
                    self.unsure(st, node, "This step was started before Aqua stopped, and it is not known whether it finished");
                    return Ok(true);
                }
                None => {
                    // The ledger line is what stops a step that reaches outside Aqua from being done twice after a crash. If it cannot
                    // be written, the step is not run: doing it without the line is how it would be repeated.
                    if let Err(e) = self.store.effect_begin(&key, run_id, &node.id, now()) {
                        tracing::error!("flow run {run_id}: the ledger line for step '{}' could not be saved: {e}", node.id);
                        self.fail_node(st, node, &format!("state could not be saved, so this step was not run: {e}"), 1, failure, false);
                        return Ok(true);
                    }
                }
            }
        }
        let Some(exec) = self.execs.get(node.kind).cloned() else {
            self.fail_node(st, node, &format!("there is nothing that can run a \"{}\" step yet", registry::spec(node.kind).label), 1, failure, false);
            return Ok(true);
        };
        running.insert(node.id.clone());
        let (node2, tz, tx2, cancel) = (node.clone(), def.settings.timezone.clone(), tx.clone(), &control.cancel);
        // A run nobody is watching (a schedule) gets the supervisor's safe retries on top of the step's own.
        let auto = st.run.trigger_kind == "schedule";
        let this = self;
        scope.spawn(move || {
            // Per thread, like the driver's own guard (`FlowRunner::start`): a thread of its own does not inherit it, so a step's tool call that needed
            // an approval waited a minute for a prompt instead of being refused at once, and an "approve for this session" answer to it made a grant
            // that the agent and the API then used. The flow's own Approval steps are how a person is asked.
            let _unattended = aqua_mcp_adapters::browser::unattended();
            // A step that panics must still report back. The driver waits for every running step, and it keeps a sender of its
            // own alive, so a worker that died silently would leave the run "running" forever with Stop unable to end it (and
            // every later scheduled time of that flow skipped). A step that reaches outside Aqua and then crashes may have done
            // its work, so it is reported as "unknown" and never retried on its own; any other crash is a plain failure.
            let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| this.run_with_retries(exec.as_ref(), run_id, &node2, cfg, &tz, cancel, auto))) {
                Ok(o) => o,
                Err(payload) => {
                    let what = payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_default();
                    tracing::error!("flow run {run_id}: step '{}' panicked: {what}", node2.id);
                    if registry::ledgered(&node2) {
                        Outcome::Failed(NodeError::Unknown("the step crashed unexpectedly, so it is not known whether it took effect".into()), 1)
                    } else {
                        Outcome::Failed(NodeError::Fatal("the step crashed unexpectedly (details are in Aqua's log)".into()), 1)
                    }
                }
            };
            let _ = tx2.send((node2.id.clone(), outcome));
        });
        Ok(true)
    }

    /// The worker's loop: try, wait, try again, according to the step's retry settings.
    fn run_with_retries(&self, exec: &dyn super::exec::NodeExecutor, run_id: i64, node: &Node, cfg: Value, tz: &str, cancel: &AtomicBool, auto: bool) -> Outcome {
        let mut attempts = 0u32;
        let mut patient = false;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Outcome::Cancelled;
            }
            attempts += 1;
            let ctx = NodeCtx { run_id, node, config: cfg.clone(), cancel, timezone: tz };
            // A click or upload that reaches this point was approved by a person (an Approval step, the step's own
            // prompt, or the flow's "don't ask" setting), so the browser may not ask again, and cannot in an unattended
            // run. It covers only this step's action; unknown sites and desktop tools are still refused.
            let _approved = registry::high_impact(node).then(aqua_mcp_adapters::browser::pre_approved);
            match exec.execute(&ctx) {
                Ok(r) => {
                    // A step that read a page, a site's comments or another service's answer hands on text from outside: the run counts as having read it from now on, so a later step
                    // that changes something asks (card OA8-04, SE-12; A6-17).
                    if registry::reads_outside_text(node) {
                        self.execs.note_outside_text(run_id);
                    }
                    return Outcome::Done(r, attempts);
                }
                Err(NodeError::Retryable(m)) if attempts <= node.retry.retries => {
                    let base = node.retry.delay_secs.max(1);
                    let delay = match node.retry.backoff {
                        Backoff::Fixed => base,
                        Backoff::Exponential => base.saturating_mul(1u64 << (attempts - 1).min(10)),
                    }
                    .min(MAX_BACKOFF_SECS);
                    self.ev(run_id, Some(&node.id), Level::Warn, "retry", &format!("\"{}\" failed ({m}); trying again in {delay}s (attempt {} of {})", node.name, attempts + 1, node.retry.retries + 1));
                    if !(self.sleep)(delay, cancel) {
                        return Outcome::Cancelled;
                    }
                }
                // The step's own retries are used up. An unattended run may still retry what is transient by nature
                // (a dropped connection, a busy service), a bounded number of times, and says which kind it saw.
                Err(NodeError::Retryable(m)) if auto && ladder_action(&m, attempts - node.retry.retries.min(attempts - 1)).is_some() => {
                    let action = ladder_action(&m, attempts - node.retry.retries.min(attempts - 1)).expect("checked");
                    use crate::flows::supervisor::Action;
                    match action {
                        Action::Retry { after_secs } => {
                            self.ev(run_id, Some(&node.id), Level::Warn, "retry", &format!("\"{}\" failed ({m}); the supervisor is trying again in {after_secs}s", node.name));
                            if !(self.sleep)(after_secs, cancel) {
                                return Outcome::Cancelled;
                            }
                        }
                        Action::ResearchDocs => {
                            // Researching a failure in the background is not built: nothing is started here, so the log says only what is true and the step fails with its own error.
                            self.ev(run_id, Some(&node.id), Level::Warn, "research", &research_not_built(&node.name, &m));
                            return Outcome::Failed(NodeError::Retryable(m), attempts);
                        }
                        _ => {
                            return Outcome::Failed(NodeError::Retryable(m), attempts);
                        }
                    }
                }
                // The ladder is spent. A fix that has proven itself for this kind of failure may add one more,
                // longer wait. Untrusted fixes are never used unasked.
                Err(NodeError::Retryable(m)) if auto && !patient && self.patience(&m) => {
                    patient = true;
                    let class = crate::flows::supervisor::classify(&m);
                    self.ev(run_id, Some(&node.id), Level::Warn, "temp_fix", &format!("\"{}\" is still failing ({} - {m}); trying once more after a longer wait, as has worked before [extra_patience/{}]", node.name, class.label(), class.key()));
                    if !(self.sleep)(EXTRA_PATIENCE_SECS, cancel) {
                        return Outcome::Cancelled;
                    }
                }
                Err(e) => return Outcome::Failed(e, attempts),
            }
        }
    }

    fn patience(&self, message: &str) -> bool {
        let class = crate::flows::supervisor::classify(message);
        class.transient() && self.store.kb_trusted(crate::flows::supervisor::recovery::EXTRA_PATIENCE, class.key())
    }

    fn succeed(&self, st: &mut State, node: &Node, outputs: Map<String, Value>, taken: Vec<String>, note: &str) {
        let id = node.id.clone();
        self.finish_node(st, &id, |n| {
            n.status = NodeStatus::Succeeded;
            n.outputs = outputs;
            n.taken = taken;
            n.finished_at = now();
            n.wait = None;
            n.error.clear();
            n.note = note.to_string();
            n.looping = None;
        });
        self.ev(st.run.id, Some(&node.id), Level::Info, "node_succeeded", &if note.is_empty() { format!("\"{}\" finished", node.name) } else { format!("\"{}\" finished: {note}", node.name) });
    }

    fn wait(&self, st: &mut State, node: &Node, w: WaitRequest, msg: &str) {
        self.wait_with_outputs(st, node, w, Map::new(), msg);
    }

    fn wait_with_outputs(&self, st: &mut State, node: &Node, w: WaitRequest, outputs: Map<String, Value>, msg: &str) {
        let id = node.id.clone();
        self.finish_node(st, &id, |n| {
            n.status = NodeStatus::Waiting;
            n.wait = Some(w);
            if !outputs.is_empty() {
                n.outputs = outputs;
            }
        });
        self.ev(st.run.id, Some(&node.id), Level::Info, "waiting", &format!("\"{}\": {msg}", node.name));
    }

    fn unsure(&self, st: &mut State, node: &Node, why: &str) {
        let id = node.id.clone();
        let message = format!("{why}. Check whether it happened (for example on the site), then choose \"It was done\" or \"Try again\".");
        self.finish_node(st, &id, |n| {
            n.status = NodeStatus::NeedsAction;
            n.error = why.to_string();
            n.wait = Some(WaitRequest::Failure { message, choices: vec!["done".into(), "retry".into(), "skip".into(), "stop".into()] });
        });
        self.ev(st.run.id, Some(&node.id), Level::Warn, "unsure", &format!("\"{}\": {why}", node.name));
    }

    /// A step failed for good: what happens next is the step's own setting.
    fn fail_node(&self, st: &mut State, node: &Node, error: &str, attempts: u32, failure: &mut Option<String>, unknown: bool) {
        let id = node.id.clone();
        let run_id = st.run.id;
        let error = crate::memory::experience::sanitize::redact(error).0;
        if unknown {
            self.unsure(st, node, &error);
            return;
        }
        // A step inside a For each step says which item it failed on.
        let at = st.position(&id).map(|p| format!(" on {p}")).unwrap_or_default();
        match node.on_error {
            OnError::Continue => {
                self.finish_node(st, &id, |n| {
                    n.status = NodeStatus::Failed;
                    n.attempts = attempts;
                    n.error = error.clone();
                    n.finished_at = now();
                    n.outputs = [("ok".to_string(), Value::Bool(false)), ("error".to_string(), Value::String(error.clone()))].into_iter().collect();
                    n.taken = vec![exit_after_error(node.kind)];
                    n.looping = None;
                });
                let way = if node.kind == NodeKind::Condition { " by its \"false\" exit (it could not be judged)" } else { "" };
                self.ev(run_id, Some(&id), Level::Warn, "node_failed", &format!("\"{}\" failed{at} and the flow carries on{way}: {error}", node.name));
            }
            OnError::AskUser => {
                self.finish_node(st, &id, |n| {
                    n.status = NodeStatus::NeedsAction;
                    n.attempts = attempts;
                    n.error = error.clone();
                    n.wait = Some(WaitRequest::Failure { message: format!("\"{}\" failed{at}: {error}", node.name), choices: vec!["retry".into(), "skip".into(), "stop".into()] });
                    n.looping = None;
                });
                self.ev(run_id, Some(&id), Level::Error, "needs_action", &format!("\"{}\" failed{at} and needs you: {error}", node.name));
            }
            OnError::Fail => {
                self.finish_node(st, &id, |n| {
                    n.status = NodeStatus::Failed;
                    n.attempts = attempts;
                    n.error = error.clone();
                    n.finished_at = now();
                    n.taken = vec![];
                    n.looping = None;
                });
                self.ev(run_id, Some(&id), Level::Error, "node_failed", &format!("\"{}\" failed{at}: {error}", node.name));
                if failure.is_none() {
                    *failure = Some(format!("\"{}\" failed{at}: {error}", node.name));
                }
            }
        }
    }

    /// Take the ledger line back after a step that did not take effect, so a retry may run it. If that cannot be written the line
    /// stays, and a retry will ask the person first: the safe side.
    fn clear_effect(&self, run_id: i64, node: &Node, key: &str) {
        if let Err(e) = self.store.effect_clear(key) {
            tracing::error!("flow run {run_id}: the ledger line for step '{}' could not be cleared: {e}", node.id);
            self.ev(run_id, Some(&node.id), Level::Warn, "effect_not_cleared", &format!("\"{}\" did not take effect, but that could not be saved ({e}). Trying it again will ask you first.", node.name));
        }
    }

    fn handle_outcome(&self, st: &mut State, def: &FlowDef, id: &str, outcome: Outcome, failure: &mut Option<String>) {
        let Some(node) = def.node(id).cloned() else { return };
        let key = st.effect_key(id);
        let risky = registry::ledgered(&node);
        match outcome {
            Outcome::Done(res, attempts) => {
                for line in &res.logs {
                    self.ev(st.run.id, Some(id), Level::Info, "log", line);
                }
                if risky {
                    // It has been done. If that cannot be written the ledger still says "started", so after a crash the person is
                    // asked rather than the step repeated; say so now.
                    if let Err(e) = self.store.effect_done(&key, &Value::Object(res.outputs.clone()), now()) {
                        tracing::error!("flow run {}: the ledger line for step '{id}' could not be marked done: {e}", st.run.id);
                        self.ev(st.run.id, Some(id), Level::Error, "effect_not_saved", &format!("\"{}\" was done, but that could not be saved ({e}). If Aqua stops now it will ask before doing it again.", node.name));
                    }
                }
                self.finish_node(st, id, |n| n.attempts = attempts);
                self.succeed(st, &node, res.outputs, vec![first_port(node.kind)], "");
            }
            Outcome::Failed(err, attempts) => {
                let unknown = matches!(err, NodeError::Unknown(_));
                if risky && !unknown {
                    self.clear_effect(st.run.id, &node, &key);
                }
                self.fail_node(st, &node, err.message(), attempts, failure, unknown);
            }
            Outcome::Cancelled => {
                if risky {
                    self.clear_effect(st.run.id, &node, &key);
                }
                self.finish_node(st, id, |n| {
                    n.status = NodeStatus::Pending;
                    n.note = "stopped".into();
                });
            }
        }
    }
}

/// Is there an enabled Approval step somewhere before this one?
pub fn has_approval_before(def: &FlowDef, id: &str) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = def.predecessors(id);
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur) {
            continue;
        }
        if def.node(cur).is_some_and(|n| n.kind == NodeKind::Approval && n.enabled) {
            return true;
        }
        stack.extend(def.predecessors(cur));
    }
    false
}
