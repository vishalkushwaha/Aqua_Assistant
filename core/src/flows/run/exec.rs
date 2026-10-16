//! What actually does the work of a step.
//!
//! The engine owns the logic steps itself (start, condition, wait, approval, question, notify, set a
//! value). Everything that reaches outside the engine (a tool, the agent, the language model, later
//! the browser, images, spreadsheets, platforms) is an executor registered here for the kinds it can
//! run. A kind with no executor cannot run, and a run says so before it starts, not half way through.

use crate::flows::model::{Node, NodeKind};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// What an executor is given: the step, its settings with every `{{ }}` already filled in, and the run.
pub struct NodeCtx<'a> {
    pub run_id: i64,
    pub node: &'a Node,
    /// The step's `config`, rendered.
    pub config: Value,
    pub cancel: &'a AtomicBool,
    /// The flow's time zone setting, for steps that deal in times.
    pub timezone: &'a str,
}

#[derive(Debug, Clone, Default)]
pub struct NodeResult {
    /// The values the step produced, by name (`summary`, `image`, `post_url`, ...).
    pub outputs: Map<String, Value>,
    /// Lines for the run's log.
    pub logs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeError {
    /// Trying again might work (a timeout, a busy service).
    Retryable(String),
    /// Trying again cannot help (bad settings, a refusal, a missing account).
    Fatal(String),
    /// It may or may not have happened (the connection dropped after sending). Never retried on its own.
    Unknown(String),
}

impl NodeError {
    pub fn message(&self) -> &str {
        match self {
            NodeError::Retryable(m) | NodeError::Fatal(m) | NodeError::Unknown(m) => m,
        }
    }
}

pub trait NodeExecutor: Send + Sync {
    /// The kinds of step this executor runs.
    fn kinds(&self) -> &[NodeKind];
    fn execute(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError>;
    /// The run `run_id` is over (it finished, failed or was stopped): whatever was kept for it can go. An executor that keeps nothing by run says nothing.
    fn run_ended(&self, _run_id: i64) {}
    /// The run `run_id` has been handed text from outside by a step that does not go through the tool host (a page the browser read, another service's answer): from now on it
    /// counts as having read it (card OA8-04, SE-12). An executor that keeps nothing by run says nothing.
    fn note_outside_text(&self, _run_id: i64) {}
}

#[derive(Default, Clone)]
pub struct Executors {
    by_kind: HashMap<NodeKind, Arc<dyn NodeExecutor>>,
}

/// Steps the engine runs itself. (The For each step is one: it has no worker, the steps inside it are the work; see `engine::loops`.)
pub const BUILT_IN: &[NodeKind] = &[NodeKind::Trigger, NodeKind::SetVariable, NodeKind::Condition, NodeKind::Loop, NodeKind::Delay, NodeKind::Schedule, NodeKind::Approval, NodeKind::UserInput, NodeKind::Notification];

impl Executors {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, e: Arc<dyn NodeExecutor>) -> Self {
        for k in e.kinds() {
            self.by_kind.insert(*k, e.clone());
        }
        self
    }

    pub fn get(&self, kind: NodeKind) -> Option<&Arc<dyn NodeExecutor>> {
        self.by_kind.get(&kind)
    }

    pub fn supports(&self, kind: NodeKind) -> bool {
        BUILT_IN.contains(&kind) || self.by_kind.contains_key(&kind)
    }

    /// Tell every executor (once each, though one may run several kinds) that the run is over.
    pub fn run_ended(&self, run_id: i64) {
        self.each(|e| e.run_ended(run_id));
    }

    /// Tell every executor (once each) that the run has been handed text from outside (see [`NodeExecutor::note_outside_text`]).
    pub fn note_outside_text(&self, run_id: i64) {
        self.each(|e| e.note_outside_text(run_id));
    }

    fn each(&self, f: impl Fn(&Arc<dyn NodeExecutor>)) {
        let mut told: Vec<*const ()> = Vec::new();
        for e in self.by_kind.values() {
            let id = Arc::as_ptr(e) as *const ();
            if !told.contains(&id) {
                told.push(id);
                f(e);
            }
        }
    }
}
