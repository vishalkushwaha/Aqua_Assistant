//! Starts runs and answers to them: the one place that changes a run from outside its driver.
//!
//! A run has at most one driver at a time. Everything a person does to a run (answer a question, pause,
//! stop, retry a step, skip it, run from a step) goes through here, checks that it makes sense for the
//! run's state, writes it to the database, and starts a driver again if there is something to drive.

use super::engine::{real_sleep, Control, Engine, Sleeper};
use super::exec::Executors;
use super::model::*;
use super::store::RunStore;
use crate::flows::model::*;
use crate::flows::registry;
use crate::flows::store::FlowStore;
use crate::flows::validate::validate;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

pub type R<T> = Result<T, String>;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn first_port(kind: NodeKind) -> String {
    super::engine::default_exit(kind)
}

/// Told whenever a run stops (finished, failed, waiting for a person, interrupted). The notifier and,
/// later, the supervisor are observers; the engine does not know about them.
pub trait RunObserver: Send + Sync {
    fn on_run_status(&self, run: &Run, nodes: &[NodeRun], status: RunStatus);
}

/// Can a run be paused now? Only one that is running: its driver is on the list of active ones **and** its record does not say that it waits. The driver writes
/// "waiting" to the record and only afterwards leaves the list, so a pause that arrived between the two was taken for a run that no longer runs, and the person was told
/// "paused" about a run that was waiting for them (found as a test that failed one time in six on a busy machine).
pub(crate) fn can_pause(status: RunStatus, driver_is_active: bool) -> R<()> {
    if driver_is_active && status != RunStatus::Waiting {
        Ok(())
    } else {
        Err(format!("this run is {}, not running", status.as_str()))
    }
}

pub struct FlowRunner {
    pub engine: Arc<Engine>,
    flows: Arc<FlowStore>,
    active: Mutex<HashMap<i64, Arc<Control>>>,
    observers: Mutex<Vec<Arc<dyn RunObserver>>>,
    /// Runs whose observers are still being told; counted as busy so "idle" means everything is done.
    announcing: std::sync::atomic::AtomicUsize,
}

impl FlowRunner {
    pub fn new(flows: Arc<FlowStore>, store: Arc<RunStore>, execs: Arc<Executors>, sleep: Arc<Sleeper>) -> Arc<Self> {
        Self::with_clock(flows, store, execs, sleep, Arc::new(now))
    }

    pub fn with_clock(flows: Arc<FlowStore>, store: Arc<RunStore>, execs: Arc<Executors>, sleep: Arc<Sleeper>, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { engine: Arc::new(Engine { store, execs, sleep, clock }), flows, active: Mutex::new(HashMap::new()), observers: Mutex::new(Vec::new()), announcing: std::sync::atomic::AtomicUsize::new(0) })
    }

    pub fn store(&self) -> &Arc<RunStore> {
        &self.engine.store
    }

    pub fn observe(&self, o: Arc<dyn RunObserver>) {
        self.observers.lock().unwrap_or_else(PoisonError::into_inner).push(o);
    }

    /// Tell the observers where a run has got to. Called when a driver stops.
    fn announce(&self, owner: &str, run_id: i64) {
        let observers: Vec<Arc<dyn RunObserver>> = self.observers.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if observers.is_empty() {
            return;
        }
        let (Ok(Some(run)), Ok(nodes)) = (self.engine.store.get_run(owner, run_id), self.engine.store.node_runs(run_id)) else { return };
        for o in observers {
            o.on_run_status(&run, &nodes, run.status);
        }
    }

    pub fn is_active(&self, run_id: i64) -> bool {
        self.active.lock().unwrap_or_else(PoisonError::into_inner).contains_key(&run_id)
    }

    pub fn active_count(&self) -> usize {
        self.active.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Start a driver for the run unless one is already going.
    fn spawn_driver(self: &Arc<Self>, owner: &str, run_id: i64) {
        let control = {
            let mut a = self.active.lock().unwrap_or_else(PoisonError::into_inner);
            if a.contains_key(&run_id) {
                return;
            }
            let c = Arc::new(Control::default());
            a.insert(run_id, c.clone());
            c
        };
        let (this, owner) = (Arc::clone(self), owner.to_string());
        let spawned = std::thread::Builder::new().name(format!("aqua-flow-run-{run_id}")).spawn({
            let (this, control, owner) = (Arc::clone(&this), control.clone(), owner.clone());
            move || {
                // Per thread: any approval prompt of the tool host is refused at once. The flow's own
                // approval steps are how a person is asked.
                let _unattended = aqua_mcp_adapters::browser::unattended();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| this.engine.drive(&owner, run_id, &control)));
                match outcome {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        tracing::warn!("flow run {run_id}: {e}");
                        let _ = this.engine.store.set_status(run_id, RunStatus::Failed, &format!("the run could not go on: {e}"), now());
                    }
                    Err(_) => {
                        let _ = this.engine.store.set_status(run_id, RunStatus::Failed, "the run crashed (contained)", now());
                    }
                }
                this.announcing.fetch_add(1, Ordering::SeqCst);
                this.driver_finished(&owner, run_id, &control);
                this.announcing.fetch_sub(1, Ordering::SeqCst);
            }
        });
        if spawned.is_err() {
            this.active.lock().unwrap_or_else(PoisonError::into_inner).remove(&run_id);
            let _ = this.engine.store.set_status(run_id, RunStatus::Failed, "could not start a thread for the run", now());
        }
    }

    /// A driver is done: it leaves the list of active ones and the observers are told. The engine has written how the run stands ("waiting", "paused", finished) before
    /// it gets here, so a stop that came after that set a flag that nothing reads any more, and the run stayed as it was: stopped by nobody. The flag is read under the lock
    /// that the driver is removed under, and `stop` sets it under the same lock, so a stop is either seen here (and done here) or finds no driver and cancels the run itself.
    fn driver_finished(&self, owner: &str, run_id: i64, control: &Control) {
        let stopped_late = {
            let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
            active.remove(&run_id);
            control.cancel.load(Ordering::SeqCst)
        };
        if stopped_late {
            if let Ok(Some(run)) = self.engine.store.get_run(owner, run_id) {
                if !run.status.is_finished() {
                    let _ = self.cancel_now(&run, "You stopped the run");
                }
            }
        }
        self.announce(owner, run_id);
    }

    /// A driver that is on the list and does nothing, for the test of a stop that comes while one is leaving.
    #[cfg(test)]
    pub(crate) fn driver_for_test(&self, run_id: i64) -> Arc<Control> {
        let control = Arc::new(Control::default());
        self.active.lock().unwrap_or_else(PoisonError::into_inner).insert(run_id, control.clone());
        control
    }

    /// [`FlowRunner::driver_finished`], for the same test.
    #[cfg(test)]
    pub(crate) fn driver_finished_for_test(&self, owner: &str, run_id: i64, control: &Control) {
        self.driver_finished(owner, run_id, control);
    }

    // ---- starting -------------------------------------------------------------------------------------

    /// Start a run of a flow. The flow must validate and every step must have something that can run it
    /// (unless this is a dry run, which simulates anything external). Inputs must be complete.
    pub fn start(self: &Arc<Self>, owner: &str, flow_id: i64, inputs: Map<String, Value>, payload: Value, dry_run: bool, trigger_kind: &str) -> R<Run> {
        self.start_keyed(owner, flow_id, inputs, payload, dry_run, trigger_kind, None).map(|(run, _)| run)
    }

    /// [`FlowRunner::start`] with a key for the start, `(key, fingerprint)`. A key that started a run before (for the same request: `fingerprint` says what that is) starts nothing new: it
    /// answers with that run and `true`, so a caller that did not hear the first answer (a timeout, a retry), or a scheduler that did not manage to write down that it had started
    /// a time, cannot make the run twice. The same key for another request is an error ([`super::store::KEY_MISMATCH`]).
    #[allow(clippy::too_many_arguments)]
    pub fn start_keyed(self: &Arc<Self>, owner: &str, flow_id: i64, inputs: Map<String, Value>, payload: Value, dry_run: bool, trigger_kind: &str, key: Option<(&str, &str)>) -> R<(Run, bool)> {
        // A start that was made before comes first, before the flow is looked at again (it may have been changed since).
        if let Some((k, fingerprint)) = key {
            if let Some(run) = self.engine.store.run_for_key(owner, flow_id, k, fingerprint)? {
                return Ok((run, true));
            }
        }
        let flow = self.flows.get(owner, flow_id)?.ok_or_else(|| "no such flow".to_string())?;
        let report = validate(&flow.def);
        if !report.ok() {
            let first: Vec<String> = report.errors().take(3).map(|i| i.message.clone()).collect();
            return Err(format!("the flow has problems and cannot run: {}", first.join(" · ")));
        }
        if !dry_run {
            let missing = self.engine.unsupported(&flow.def);
            if !missing.is_empty() {
                crate::self_repair::note_refused_steps(flow_id, &flow.def.name, &flow.def, &missing);
                return Err(format!("these steps cannot run yet in this version: {}. Nothing was started. Try a dry run, change the platform to one that works, or switch them off.", missing.join(", ")));
            }
        }
        let mut vars = Map::new();
        let mut absent: Vec<String> = Vec::new();
        for v in &flow.def.variables {
            match inputs.get(&v.name).filter(|x| !x.is_null() && x.as_str().is_none_or(|s| !s.trim().is_empty())).cloned().or_else(|| v.default.clone()) {
                Some(x) => {
                    if v.kind == VarKind::Choice && !v.choices.is_empty() && x.as_str().is_some_and(|s| !v.choices.iter().any(|c| c == s)) {
                        return Err(format!("\"{}\" must be one of: {}", v.label_or_name(), v.choices.join(", ")));
                    }
                    vars.insert(v.name.clone(), x);
                }
                None if v.required => absent.push(v.label_or_name()),
                None => {}
            }
        }
        if !absent.is_empty() {
            return Err(format!("missing: {}", absent.join(", ")));
        }
        // The names of the inputs that came from text the caller had read from outside: only of inputs that this run has (card R6-11).
        match inputs.get(OUTSIDE_VARS) {
            Some(Value::Array(names)) => {
                let known: Vec<Value> = names.iter().filter_map(|n| n.as_str()).filter(|n| vars.contains_key(*n)).map(|n| Value::String(n.to_string())).collect();
                if !known.is_empty() {
                    vars.insert(OUTSIDE_VARS.to_string(), Value::Array(known));
                }
            }
            // A note that is there and is not a list cannot be read: every input counts as outside.
            Some(_) => {
                let all: Vec<Value> = vars.keys().map(|k| Value::String(k.clone())).collect();
                if !all.is_empty() {
                    vars.insert(OUTSIDE_VARS.to_string(), Value::Array(all));
                }
            }
            None => {}
        }
        let (run, duplicate) = self.engine.store.create_run_keyed(owner, &flow, vars, payload, trigger_kind, dry_run, None, now(), key)?;
        if duplicate {
            return Ok((run, true));
        }
        let _ = self.flows.audit(owner, flow_id, "user", if dry_run { "dry_run_started" } else { "run_started" }, &format!("run {}", run.id), now());
        self.spawn_driver(owner, run.id);
        Ok((run, false))
    }

    // ---- what a person does to a run ---------------------------------------------------------------------

    fn get(&self, owner: &str, run_id: i64) -> R<Run> {
        self.engine.store.get_run(owner, run_id)?.ok_or_else(|| "no such run".to_string())
    }

    /// The ledger key of a step of a run: one for each item for a step inside a For each step (card R7-12), the old `run:step` for any other.
    fn effect_key(&self, run: &Run, node_id: &str) -> String {
        let store = &self.engine.store;
        let item = crate::flows::looping::item_of(&run.def, node_id, |l| store.node_run(run.id, l).ok().flatten().and_then(|n| n.looping).map(|s| s.index));
        crate::flows::looping::effect_key(run.id, node_id, item)
    }

    fn cancel_now(&self, run: &Run, why: &str) -> R<()> {
        for n in self.engine.store.node_runs(run.id)? {
            // "Running" with no driver is a For each step that is on an item (no worker runs it); a stop ends it with the rest.
            if matches!(n.status, NodeStatus::Pending | NodeStatus::Waiting | NodeStatus::NeedsAction | NodeStatus::Interrupted | NodeStatus::Running) {
                let mut n = n;
                n.status = NodeStatus::Skipped;
                n.taken = vec![];
                n.wait = None;
                n.note = why.to_string();
                self.engine.store.save_node(&n)?;
            }
        }
        self.engine.store.set_status(run.id, RunStatus::Cancelled, why, now())?;
        let _ = self.engine.store.add_event(run.id, None, Level::Info, "run_cancelled", why, now());
        Ok(())
    }

    /// Answer a step that is waiting for a person.
    pub fn respond(self: &Arc<Self>, owner: &str, run_id: i64, node_id: &str, resp: Response) -> R<()> {
        let run = self.get(owner, run_id)?;
        if run.status.is_finished() {
            return Err("this run has finished".into());
        }
        let node = run.def.node(node_id).ok_or_else(|| "no such step in this run".to_string())?.clone();
        let mut nr = self.engine.store.node_run(run_id, node_id)?.ok_or_else(|| "no such step in this run".to_string())?;
        let wait = nr.wait.clone().filter(|_| matches!(nr.status, NodeStatus::Waiting | NodeStatus::NeedsAction)).ok_or_else(|| "this step is not waiting for you".to_string())?;
        let ev = |level: Level, kind: &str, msg: &str| {
            let _ = self.engine.store.add_event(run_id, Some(node_id), level, kind, msg, now());
        };
        match wait {
            WaitRequest::Approval { implicit, allow_edit, preview, .. } => {
                let decision = resp.decision.as_deref().ok_or("say whether you approve or reject it")?;
                match (decision, implicit) {
                    ("approved", true) => {
                        nr.gate_approved = true;
                        nr.status = NodeStatus::Pending;
                        nr.wait = None;
                        ev(Level::Info, "approved", &format!("You approved \"{}\"", node.name));
                    }
                    ("rejected", true) => {
                        self.engine.store.add_event(run_id, Some(node_id), Level::Info, "rejected", &format!("You did not approve \"{}\"", node.name), now())?;
                        return self.cancel_now(&run, &format!("You did not approve \"{}\"", node.name));
                    }
                    ("approved" | "rejected", false) => {
                        let edits = resp.edits.clone().filter(|e| allow_edit && !e.trim().is_empty());
                        let mut o = Map::new();
                        o.insert("decision".into(), json!(decision));
                        o.insert("edits".into(), json!(edits.clone().unwrap_or_default()));
                        o.insert("text".into(), json!(edits.clone().unwrap_or(preview)));
                        nr.status = NodeStatus::Succeeded;
                        nr.outputs = o;
                        nr.taken = vec![decision.to_string()];
                        nr.finished_at = now();
                        nr.wait = None;
                        ev(Level::Info, decision, &format!("You {} \"{}\"{}", decision, node.name, if edits.is_some() { " with your edits" } else { "" }));
                    }
                    _ => return Err("the decision must be \"approved\" or \"rejected\"".into()),
                }
            }
            WaitRequest::Input { answer_kind, options, .. } => {
                let raw = resp.answer.as_deref().map(str::trim).filter(|a| !a.is_empty()).ok_or("please answer")?;
                let answer = match answer_kind.as_str() {
                    "yes_no" => match raw.to_ascii_lowercase().as_str() {
                        "yes" | "y" | "true" | "1" | "ok" | "sure" => "yes".to_string(),
                        "no" | "n" | "false" | "0" => "no".to_string(),
                        _ => return Err("answer yes or no".into()),
                    },
                    "number" => raw.parse::<f64>().map_err(|_| "answer with a number".to_string())?.to_string(),
                    "choice" if !options.is_empty() && !options.iter().any(|o| o == raw) => return Err(format!("choose one of: {}", options.join(", "))),
                    _ => raw.to_string(),
                };
                nr.status = NodeStatus::Succeeded;
                nr.outputs = [("answer".to_string(), json!(answer))].into_iter().collect();
                nr.taken = vec![first_port(node.kind)];
                nr.finished_at = now();
                nr.wait = None;
                ev(Level::Info, "answered", &format!("You answered \"{}\": {}", node.name, answer));
            }
            WaitRequest::Failure { choices, .. } => {
                let choice = resp.choice.as_deref().ok_or("choose what to do")?;
                if !choices.iter().any(|c| c == choice) {
                    return Err(format!("choose one of: {}", choices.join(", ")));
                }
                let key = self.effect_key(&run, node_id);
                match choice {
                    "retry" => {
                        let _ = self.engine.store.effect_clear(&key);
                        nr.status = NodeStatus::Pending;
                        nr.attempts = 0;
                        nr.error.clear();
                        nr.wait = None;
                        ev(Level::Info, "retry", &format!("You chose to try \"{}\" again", node.name));
                    }
                    "skip" => {
                        nr.status = NodeStatus::Skipped;
                        nr.outputs = [("ok".to_string(), json!(false)), ("skipped".to_string(), json!(true))].into_iter().collect();
                        nr.taken = vec![first_port(node.kind)];
                        nr.finished_at = now();
                        nr.wait = None;
                        nr.note = "skipped by you".into();
                        ev(Level::Info, "skipped", &format!("You skipped \"{}\"", node.name));
                    }
                    "done" => {
                        let mut o = Map::new();
                        for out in &node.outputs {
                            o.insert(out.clone(), json!(if out == "status" { "done (confirmed by you)" } else { "" }));
                        }
                        if let Some(a) = resp.answer.as_deref().filter(|a| !a.trim().is_empty()) {
                            o.insert("post_url".into(), json!(a.trim()));
                        }
                        let _ = self.engine.store.effect_done(&key, &Value::Object(o.clone()), now());
                        nr.status = NodeStatus::Succeeded;
                        nr.outputs = o;
                        nr.taken = vec![first_port(node.kind)];
                        nr.finished_at = now();
                        nr.wait = None;
                        nr.note = "you confirmed it was done".into();
                        ev(Level::Info, "confirmed", &format!("You confirmed \"{}\" was done", node.name));
                    }
                    _ => return self.cancel_now(&run, "You stopped the run"),
                }
            }
            WaitRequest::Until { .. } => return Err("this step is waiting for a time, not for you".into()),
        }
        self.engine.store.save_node(&nr)?;
        self.spawn_driver(owner, run_id);
        Ok(())
    }

    pub fn pause(&self, owner: &str, run_id: i64) -> R<()> {
        let run = self.get(owner, run_id)?;
        let a = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        let control = a.get(&run_id);
        can_pause(run.status, control.is_some())?;
        if let Some(c) = control {
            c.pause.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Carry on a run that is paused, interrupted, or waiting on a time that has come.
    pub fn resume(self: &Arc<Self>, owner: &str, run_id: i64) -> R<()> {
        let run = self.get(owner, run_id)?;
        if run.status.is_finished() {
            return Err("this run has finished".into());
        }
        if self.is_active(run_id) {
            if let Some(c) = self.active.lock().unwrap_or_else(PoisonError::into_inner).get(&run_id) {
                c.pause.store(false, Ordering::SeqCst);
            }
            return Ok(());
        }
        for n in self.engine.store.node_runs(run_id)? {
            if n.status != NodeStatus::Interrupted {
                continue;
            }
            let Some(node) = run.def.node(&n.node_id) else { continue };
            let key = self.effect_key(&run, &n.node_id);
            let mut n = n;
            let risky = registry::ledgered(node);
            match (risky, self.engine.store.effect_get(&key)?) {
                (true, Some((s, result))) if s == "done" => {
                    n.status = NodeStatus::Succeeded;
                    n.outputs = result.as_object().cloned().unwrap_or_default();
                    n.taken = vec![first_port(node.kind)];
                    n.finished_at = now();
                    n.note = "it had already been done; not repeated".into();
                }
                (true, Some(_)) => {
                    n.status = NodeStatus::NeedsAction;
                    let why = "Aqua stopped while this step was running, and it is not known whether it finished";
                    n.error = why.into();
                    n.wait = Some(WaitRequest::Failure { message: format!("{why}. Check whether it happened, then choose \"It was done\" or \"Try again\"."), choices: vec!["done".into(), "retry".into(), "skip".into(), "stop".into()] });
                }
                _ => {
                    // A step with no outside effect is safe to run again.
                    n.status = NodeStatus::Pending;
                    n.error.clear();
                    n.attempts = 0;
                }
            }
            self.engine.store.save_node(&n)?;
        }
        let _ = self.engine.store.add_event(run_id, None, Level::Info, "resume", "Resuming", now());
        self.spawn_driver(owner, run_id);
        Ok(())
    }

    pub fn stop(&self, owner: &str, run_id: i64) -> R<()> {
        let run = self.get(owner, run_id)?;
        if run.status.is_finished() {
            return Err("this run has finished".into());
        }
        if let Some(c) = self.active.lock().unwrap_or_else(PoisonError::into_inner).get(&run_id) {
            c.cancel.store(true, Ordering::SeqCst);
            return Ok(());
        }
        self.cancel_now(&run, "You stopped the run")
    }

    /// The same flow, the same inputs, from the beginning, as a new run.
    pub fn restart(self: &Arc<Self>, owner: &str, run_id: i64) -> R<Run> {
        let run = self.get(owner, run_id)?;
        if !run.status.is_finished() && !matches!(run.status, RunStatus::Interrupted | RunStatus::Paused) {
            return Err("stop this run first, or wait for it to finish".into());
        }
        self.start(owner, run.flow_id, run.vars.clone(), run.payload.clone(), run.dry_run, &run.trigger_kind)
    }

    /// Steps that were skipped only because the run failed before them go back to pending.
    fn reopen(&self, run: &Run) -> R<()> {
        for n in self.engine.store.node_runs(run.id)? {
            // Skipped because of where the run had got to (a failed step before it, a path not taken, a stop):
            // it goes back to pending and the engine decides again. A step skipped on purpose stays skipped.
            if n.status == NodeStatus::Skipped && (n.note == "the run failed before it" || n.note == "the run was stopped" || n.note == "the path to it was not taken") {
                let mut n = n;
                n.status = NodeStatus::Pending;
                n.note.clear();
                n.taken.clear();
                self.engine.store.save_node(&n)?;
            }
        }
        Ok(())
    }

    /// Try a failed step again, then carry on from it.
    pub fn retry_node(self: &Arc<Self>, owner: &str, run_id: i64, node_id: &str) -> R<()> {
        let run = self.get(owner, run_id)?;
        if self.is_active(run_id) {
            return Err("the run is going; wait for it to stop or pause it".into());
        }
        let mut nr = self.engine.store.node_run(run_id, node_id)?.ok_or("no such step in this run")?;
        if !matches!(nr.status, NodeStatus::Failed | NodeStatus::NeedsAction | NodeStatus::Interrupted) {
            return Err(format!("this step is {}, not failed", nr.status.as_str()));
        }
        let node = run.def.node(node_id).ok_or("no such step in this run")?;
        if registry::ledgered(node) {
            let _ = self.engine.store.effect_clear(&self.effect_key(&run, node_id));
        }
        nr.status = NodeStatus::Pending;
        nr.attempts = 0;
        nr.error.clear();
        nr.wait = None;
        nr.outputs.clear();
        nr.taken.clear();
        self.engine.store.save_node(&nr)?;
        self.reopen(&run)?;
        self.engine.store.set_status(run_id, RunStatus::Queued, "", now())?;
        let _ = self.engine.store.add_event(run_id, Some(node_id), Level::Info, "retry", &format!("Trying \"{}\" again", node.name), now());
        self.spawn_driver(owner, run_id);
        Ok(())
    }

    /// Skip a failed or waiting step and carry on past it.
    pub fn skip_node(self: &Arc<Self>, owner: &str, run_id: i64, node_id: &str) -> R<()> {
        let run = self.get(owner, run_id)?;
        if self.is_active(run_id) {
            return Err("the run is going; wait for it to stop or pause it".into());
        }
        let mut nr = self.engine.store.node_run(run_id, node_id)?.ok_or("no such step in this run")?;
        if !matches!(nr.status, NodeStatus::Failed | NodeStatus::NeedsAction | NodeStatus::Interrupted | NodeStatus::Waiting) {
            return Err(format!("this step is {}; only a failed or waiting step can be skipped", nr.status.as_str()));
        }
        let node = run.def.node(node_id).ok_or("no such step in this run")?;
        nr.status = NodeStatus::Skipped;
        nr.outputs = [("ok".to_string(), json!(false)), ("skipped".to_string(), json!(true))].into_iter().collect();
        nr.taken = vec![first_port(node.kind)];
        nr.finished_at = now();
        nr.wait = None;
        nr.note = "skipped by you".into();
        self.engine.store.save_node(&nr)?;
        self.reopen(&run)?;
        self.engine.store.set_status(run_id, RunStatus::Queued, "", now())?;
        let _ = self.engine.store.add_event(run_id, Some(node_id), Level::Info, "skipped", &format!("You skipped \"{}\"", node.name), now());
        self.spawn_driver(owner, run_id);
        Ok(())
    }

    /// A new run that reuses everything before `node_id` from an earlier run and does that step and what follows.
    pub fn run_from(self: &Arc<Self>, owner: &str, run_id: i64, node_id: &str) -> R<Run> {
        let src = self.get(owner, run_id)?;
        if self.is_active(run_id) || matches!(src.status, RunStatus::Running | RunStatus::Queued) {
            return Err("that run is still going".into());
        }
        if src.def.node(node_id).is_none() {
            return Err("no such step in this run".into());
        }
        // The steps inside a For each step only make sense with the item they were on; going on from one of them would leave out the rest of the list.
        if let Some(l) = crate::flows::looping::enclosing_loop(&src.def, node_id) {
            let name = src.def.node(l).map(|n| n.name.clone()).unwrap_or_else(|| l.to_string());
            return Err(format!("that step is inside the For each step \"{name}\": run from \"{name}\" instead"));
        }
        let mut later: HashSet<String> = HashSet::new();
        let mut q: VecDeque<String> = VecDeque::from([node_id.to_string()]);
        while let Some(cur) = q.pop_front() {
            if later.insert(cur.clone()) {
                q.extend(src.def.successors(&cur, None).into_iter().map(String::from));
            }
        }
        let old: HashMap<String, NodeRun> = self.engine.store.node_runs(run_id)?.into_iter().map(|n| (n.node_id.clone(), n)).collect();
        for n in &src.def.nodes {
            if !later.contains(&n.id) && !old.get(&n.id).is_some_and(|o| o.status.is_settled()) && src.def.predecessors(node_id).contains(&n.id.as_str()) {
                return Err(format!("\"{}\" did not finish in that run, so there is nothing to reuse; run it from there instead", n.name));
            }
        }
        let flow = Flow { id: src.flow_id, owner_id: owner.to_string(), version: src.flow_version, created_at: 0, updated_at: 0, def: src.def.clone() };
        let run = self.engine.store.create_run(owner, &flow, src.vars.clone(), src.payload.clone(), "run_from", src.dry_run, Some(run_id), now())?;
        for n in &src.def.nodes {
            if later.contains(&n.id) {
                continue;
            }
            if let Some(o) = old.get(&n.id).filter(|o| o.status.is_settled()) {
                let mut copy = o.clone();
                copy.run_id = run.id;
                copy.note = format!("reused from run {run_id}");
                self.engine.store.save_node(&copy)?;
            }
        }
        let _ = self.engine.store.add_event(run.id, Some(node_id), Level::Info, "run_from", &format!("Running from \"{}\", reusing the earlier results from run {run_id}", src.def.node(node_id).map(|n| n.name.clone()).unwrap_or_default()), now());
        self.spawn_driver(owner, run.id);
        Ok(run)
    }

    // ---- time --------------------------------------------------------------------------------------------

    /// Wake runs whose wait has come to an end. Returns how many.
    pub fn tick(self: &Arc<Self>, at: i64) -> usize {
        let due = match self.engine.store.due_waits(at) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("flow scheduler: {e}");
                return 0;
            }
        };
        let mut n = 0;
        for (owner, id) in due {
            if !self.is_active(id) {
                self.spawn_driver(&owner, id);
                n += 1;
            }
        }
        n
    }

    /// Mark what was going when Aqua stopped. The person decides whether to resume.
    pub fn recover(&self) -> usize {
        self.engine.store.recover_interrupted(now()).map(|v| v.len()).unwrap_or(0)
    }

    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let t = std::time::Instant::now();
        while self.active_count() > 0 || self.announcing.load(Ordering::SeqCst) > 0 {
            if t.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

impl VariableDef {
    pub fn label_or_name(&self) -> String {
        if self.label.trim().is_empty() {
            self.name.clone()
        } else {
            self.label.clone()
        }
    }
}

static RUNNER: OnceLock<Arc<FlowRunner>> = OnceLock::new();

pub fn runner() -> Option<Arc<FlowRunner>> {
    RUNNER.get().cloned()
}

/// Install the runner and start the thread that wakes waiting runs. Runs left going by a crash are
/// marked interrupted; nothing is resumed by itself.
pub fn spawn(flows: Arc<FlowStore>, store: Arc<RunStore>, execs: Arc<Executors>) -> Arc<FlowRunner> {
    let r = FlowRunner::new(flows, store, execs, Arc::new(real_sleep));
    if RUNNER.set(r.clone()).is_err() {
        return runner().expect("set");
    }
    let n = r.recover();
    if n > 0 {
        tracing::info!("{n} flow run(s) were interrupted by the last shutdown");
    }
    let this = r.clone();
    // A job of the registry (`jobs`): listed, stopped by the shutdown, and it promises a sign of life every five seconds. Not supervised, as before: a panic ends it, and the registry says so.
    let spawned = crate::jobs::spawn(crate::jobs::JobKind::Scheduler, "aqua-flow-clock", Some(Duration::from_secs(5)), move |job| {
        while job.sleep(Duration::from_secs(5)) {
            this.tick(now());
        }
        Ok(())
    });
    if spawned.is_err() {
        tracing::warn!("could not start the flow clock thread");
    }
    r
}
