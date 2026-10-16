//! What a flow is: a graph of nodes joined by edges, plus the variables it takes and its settings.
//!
//! The definition is JSON and is the only source of truth; the canvas is a view of it. Node kinds
//! and what each accepts are described in `registry`, so this file has no per-kind logic.

use super::registry;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_NODES: usize = 100;
pub const MAX_EDGES: usize = 300;
pub const MAX_DEF_BYTES: usize = 512 * 1024;
pub const MAX_RETRIES: u32 = 5;

/// The kinds of node. Adding one means an entry in `registry` and, later, an executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Trigger,
    UserInput,
    Research,
    TextGeneration,
    ImageGeneration,
    AiAgent,
    Browser,
    Website,
    Http,
    N8n,
    CommentReplies,
    Tool,
    File,
    Excel,
    Database,
    SocialMedia,
    Condition,
    Loop,
    Delay,
    Schedule,
    Approval,
    Notification,
    SetVariable,
}

impl NodeKind {
    pub const ALL: &'static [NodeKind] = &[
        NodeKind::Trigger,
        NodeKind::UserInput,
        NodeKind::Research,
        NodeKind::TextGeneration,
        NodeKind::ImageGeneration,
        NodeKind::AiAgent,
        NodeKind::Browser,
        NodeKind::Website,
        NodeKind::Http,
        NodeKind::N8n,
        NodeKind::CommentReplies,
        NodeKind::Tool,
        NodeKind::File,
        NodeKind::Excel,
        NodeKind::Database,
        NodeKind::SocialMedia,
        NodeKind::Condition,
        NodeKind::Loop,
        NodeKind::Delay,
        NodeKind::Schedule,
        NodeKind::Approval,
        NodeKind::Notification,
        NodeKind::SetVariable,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Trigger => "trigger",
            NodeKind::UserInput => "user_input",
            NodeKind::Research => "research",
            NodeKind::TextGeneration => "text_generation",
            NodeKind::ImageGeneration => "image_generation",
            NodeKind::AiAgent => "ai_agent",
            NodeKind::Browser => "browser",
            NodeKind::Website => "website",
            NodeKind::Http => "http",
            NodeKind::N8n => "n8n",
            NodeKind::CommentReplies => "comment_replies",
            NodeKind::Tool => "tool",
            NodeKind::File => "file",
            NodeKind::Excel => "excel",
            NodeKind::Database => "database",
            NodeKind::SocialMedia => "social_media",
            NodeKind::Condition => "condition",
            NodeKind::Loop => "loop",
            NodeKind::Delay => "delay",
            NodeKind::Schedule => "schedule",
            NodeKind::Approval => "approval",
            NodeKind::Notification => "notification",
            NodeKind::SetVariable => "set_variable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Backoff {
    Fixed,
    #[default]
    Exponential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default)]
    pub retries: u32,
    #[serde(default = "ten")]
    pub delay_secs: u64,
    #[serde(default)]
    pub backoff: Backoff,
}

fn ten() -> u64 {
    10
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { retries: 0, delay_secs: 10, backoff: Backoff::Exponential }
    }
}

/// What happens when a node fails for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// The run stops as failed.
    #[default]
    Fail,
    /// The failure is recorded and the run goes on down the normal path.
    Continue,
    /// The run waits for a person to retry, skip or stop.
    AskUser,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "empty_object")]
    pub config: Value,
    /// Names of the values this node reads (for the canvas and for checks); informational.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Names of the values this node produces. Empty means the kind's defaults (filled on save).
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub position: Position,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub retry: RetryPolicy,
    #[serde(default)]
    pub on_error: OnError,
    /// The name of a stored credential (see Credential Manager). Never a secret value.
    #[serde(default)]
    pub credential: Option<String>,
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    #[serde(default)]
    pub id: String,
    pub from: String,
    pub to: String,
    #[serde(default = "out_port")]
    pub port: String,
}

fn out_port() -> String {
    "out".into()
}

impl Edge {
    pub fn new(from: &str, to: &str, port: &str) -> Self {
        Self { id: Self::make_id(from, port, to), from: from.into(), to: to.into(), port: port.into() }
    }
    pub fn make_id(from: &str, port: &str, to: &str) -> String {
        format!("{from}:{port}->{to}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VarKind {
    #[default]
    Text,
    LongText,
    Number,
    Bool,
    Choice,
}

/// A run-time input of the flow (`{{vars.topic}}`), so one flow can be run for many topics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableDef {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: VarKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub choices: Vec<String>,
}

/// When publishing-type actions (high risk) ask a person first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Every run asks before a high-impact action.
    #[default]
    Always,
    /// Only the first run of a flow that is new (or was changed) asks.
    NewOnly,
    /// Never asks. High-impact actions then depend on the tool policy alone.
    Never,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FlowSettings {
    #[serde(default)]
    pub approval: ApprovalPolicy,
    /// An IANA name such as "Asia/Kolkata"; empty means this computer's zone.
    #[serde(default)]
    pub timezone: String,
}

/// The part of a flow that is versioned: everything a person edits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FlowDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub variables: Vec<VariableDef>,
    #[serde(default)]
    pub settings: FlowSettings,
}

/// A stored flow: its current definition and where it is in its history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flow {
    pub id: i64,
    pub owner_id: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(flatten)]
    pub def: FlowDef,
}

pub fn current_owner() -> &'static str {
    crate::workflows::model::current_owner()
}

impl Node {
    pub fn new(id: &str, kind: NodeKind, name: &str) -> Self {
        Node {
            id: id.into(),
            kind,
            name: name.into(),
            description: String::new(),
            config: empty_object(),
            inputs: Vec::new(),
            outputs: registry::spec(kind).default_outputs.clone(),
            position: Position::default(),
            enabled: true,
            retry: RetryPolicy::default(),
            on_error: OnError::Fail,
            credential: None,
        }
    }

    pub fn with_config(mut self, config: Value) -> Self {
        self.config = config;
        self
    }

    pub fn at(mut self, x: f64, y: f64) -> Self {
        self.position = Position { x, y };
        self
    }
}

impl FlowDef {
    /// Fill in what a person or the planner may leave out, so what is stored is self-describing:
    /// edge ids, default outputs, trimmed names. Does not change meaning.
    pub fn normalize(&mut self) {
        self.name = self.name.trim().to_string();
        for n in &mut self.nodes {
            if n.name.trim().is_empty() {
                n.name = registry::spec(n.kind).label.clone();
            }
            n.name = n.name.trim().to_string();
            if n.outputs.is_empty() {
                n.outputs = registry::spec(n.kind).default_outputs.clone();
            }
            if !n.config.is_object() && !n.config.is_null() {
                continue;
            }
            if n.config.is_null() {
                n.config = empty_object();
            }
            n.retry.retries = n.retry.retries.min(MAX_RETRIES);
        }
        for e in &mut self.edges {
            if e.port.trim().is_empty() {
                e.port = "out".into();
            }
            if e.id.trim().is_empty() {
                e.id = Edge::make_id(&e.from, &e.port, &e.to);
            }
        }
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn node_mut(&mut self, id: &str) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    /// The nodes an edge leaving `id` on `port` (or any port when `None`) leads to.
    pub fn successors<'a>(&'a self, id: &str, port: Option<&str>) -> Vec<&'a str> {
        self.edges.iter().filter(|e| e.from == id && port.is_none_or(|p| e.port == p)).map(|e| e.to.as_str()).collect()
    }

    pub fn predecessors<'a>(&'a self, id: &str) -> Vec<&'a str> {
        self.edges.iter().filter(|e| e.to == id).map(|e| e.from.as_str()).collect()
    }

    /// A cheap size check before anything is parsed further or stored.
    pub fn check_size(&self) -> Result<(), String> {
        if self.nodes.len() > MAX_NODES {
            return Err(format!("a flow can have at most {MAX_NODES} nodes"));
        }
        if self.edges.len() > MAX_EDGES {
            return Err(format!("a flow can have at most {MAX_EDGES} connections"));
        }
        let bytes = serde_json::to_vec(self).map(|v| v.len()).unwrap_or(usize::MAX);
        if bytes > MAX_DEF_BYTES {
            return Err("the flow is too large".into());
        }
        Ok(())
    }
}
