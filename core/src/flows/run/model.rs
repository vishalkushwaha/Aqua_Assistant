//! What a run of a flow looks like while it happens and afterwards.
//!
//! Everything here is stored, so a run is not a thread's memory: it can be looked at, paused, resumed
//! after Aqua restarts, and inspected long after. A run holds a *snapshot* of the flow as it was when it
//! started, so editing the flow does not change a run that is going.

use crate::flows::model::FlowDef;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Created; the driver has not started yet.
    Queued,
    Running,
    /// Nothing can go on until a person answers or a time arrives. No thread is held.
    Waiting,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
    /// Aqua stopped while it was running. It can be resumed.
    Interrupted,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Waiting => "waiting",
            RunStatus::Paused => "paused",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Interrupted => "interrupted",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "queued" => RunStatus::Queued,
            "running" => RunStatus::Running,
            "waiting" => RunStatus::Waiting,
            "paused" => RunStatus::Paused,
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            _ => RunStatus::Interrupted,
        }
    }
    pub fn is_finished(self) -> bool {
        matches!(self, RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Running,
    /// Waiting for a person or a time.
    Waiting,
    Succeeded,
    Failed,
    Skipped,
    /// Failed (or unsure whether it happened) and waiting for a person to choose what to do.
    NeedsAction,
    /// Aqua stopped while it was running: the outcome is not known.
    Interrupted,
}

impl NodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeStatus::Pending => "pending",
            NodeStatus::Running => "running",
            NodeStatus::Waiting => "waiting",
            NodeStatus::Succeeded => "succeeded",
            NodeStatus::Failed => "failed",
            NodeStatus::Skipped => "skipped",
            NodeStatus::NeedsAction => "needs_action",
            NodeStatus::Interrupted => "interrupted",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "pending" => NodeStatus::Pending,
            "running" => NodeStatus::Running,
            "waiting" => NodeStatus::Waiting,
            "succeeded" => NodeStatus::Succeeded,
            "failed" => NodeStatus::Failed,
            "skipped" => NodeStatus::Skipped,
            "needs_action" => NodeStatus::NeedsAction,
            _ => NodeStatus::Interrupted,
        }
    }
    /// The step has finished one way or another, so what leaves it is decided.
    pub fn is_settled(self) -> bool {
        matches!(self, NodeStatus::Succeeded | NodeStatus::Failed | NodeStatus::Skipped)
    }
}

/// What a step is waiting for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitRequest {
    /// Show a preview and wait for approve / reject (and, if allowed, an edited text).
    Approval {
        title: String,
        preview: String,
        allow_edit: bool,
        /// Asked by Aqua because nothing in the flow asks before a high-impact step, not by an Approval step.
        implicit: bool,
    },
    /// A question for the person.
    Input { question: String, answer_kind: String, options: Vec<String> },
    /// A time. The run resumes by itself when it comes.
    Until { ts: i64 },
    /// The step failed, or may have happened already. The person chooses.
    Failure { message: String, choices: Vec<String> },
}

/// Where a For each step is in its list. It lives in the step's record, so a run that waits for a person, is paused, or is stopped by a crash goes on at the item it
/// was on and does not do the items before it again (card R7-12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LoopState {
    /// The list, read once when the step began.
    pub items: Vec<Value>,
    /// The item the steps inside are on (counted from 0).
    pub index: usize,
    /// True until the steps inside have been put back to "not run yet" for item `index`. A crash between the two is redone from here, so an item never starts with
    /// the leftovers of the one before it.
    pub fresh: bool,
    /// One entry for each finished item: the id of each step inside that ran, and what it produced.
    pub results: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRun {
    pub run_id: i64,
    pub node_id: String,
    pub status: NodeStatus,
    pub attempts: u32,
    pub started_at: i64,
    pub finished_at: i64,
    /// The values this step produced: `{{nodes.<id>.<name>}}`.
    pub outputs: Map<String, Value>,
    /// The exits that were taken. Only edges leaving by one of these carry the run on.
    pub taken: Vec<String>,
    pub error: String,
    pub wait: Option<WaitRequest>,
    /// A person has approved this step's implicit approval.
    pub gate_approved: bool,
    /// A line for the person: why it was skipped, that it was reused from an earlier run, and so on.
    pub note: String,
    /// For a For each step that is partway through its list. Kept in the database, not sent to the app (the list can be long); the step's outputs show the item it is on.
    #[serde(skip)]
    pub looping: Option<LoopState>,
}

impl NodeRun {
    pub fn pending(run_id: i64, node_id: &str) -> Self {
        NodeRun { run_id, node_id: node_id.into(), status: NodeStatus::Pending, attempts: 0, started_at: 0, finished_at: 0, outputs: Map::new(), taken: vec![], error: String::new(), wait: None, gate_approved: false, note: String::new(), looping: None }
    }
}

/// The entry of a run's variables that lists the names of the ones that hold text from outside (set from a page, a step's output or a webhook, or given as an
/// input by a run that had read such text): a variable is as much outside as what it was made from (card R6-11). It is the run's own record, kept with the
/// variables so that it survives a restart, and a flow cannot read it as a variable (the engine takes it out of what a template sees).
pub const OUTSIDE_VARS: &str = "__outside";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: i64,
    pub flow_id: i64,
    pub owner_id: String,
    pub flow_version: i64,
    pub name: String,
    pub status: RunStatus,
    pub trigger_kind: String,
    pub dry_run: bool,
    /// The flow's inputs for this run, plus anything a "Set a value" step added.
    pub vars: Map<String, Value>,
    pub payload: Value,
    pub created_at: i64,
    pub started_at: i64,
    pub finished_at: i64,
    pub error: String,
    /// The run whose results this one reused (run from a step).
    pub from_run: Option<i64>,
    /// The flow as it was when the run started.
    pub def: FlowDef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warn,
    Error,
    /// A message for the person (the Notify step).
    Notify,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
            Level::Notify => "notify",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunEvent {
    pub id: i64,
    pub run_id: i64,
    pub at: i64,
    pub node_id: Option<String>,
    pub level: String,
    /// run_started, node_started, node_succeeded, node_failed, waiting, approved, ... (for filtering)
    pub kind: String,
    pub message: String,
}

/// Something in a run that needs a person, for the inbox.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InboxItem {
    pub run_id: i64,
    pub flow_id: i64,
    pub flow_name: String,
    pub node_id: String,
    pub node_name: String,
    pub since: i64,
    pub wait: WaitRequest,
}

/// A person's answer to a waiting step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// "approved" or "rejected" for an approval.
    #[serde(default)]
    pub decision: Option<String>,
    /// The answer to a question.
    #[serde(default)]
    pub answer: Option<String>,
    /// An edited text for an approval that allows edits.
    #[serde(default)]
    pub edits: Option<String>,
    /// For a failure: "retry", "skip", "stop" or "done".
    #[serde(default)]
    pub choice: Option<String>,
}
