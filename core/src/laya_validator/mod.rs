//! Laya gatekeeper: fast safety + intent routing before Llama / MCP.
//!
//! Not an MCP tool and not a UI observer — only decides refuse / chat / mcp_agent.

pub mod calibrate;
mod decision_route;
mod engine;
mod fallback;
pub mod feedback_stats;
mod questions;

pub use decision_route::{apply as apply_routed, combine, enabled as decision_router_enabled, refine as refine_route, refusing_enabled as decision_refusing_enabled, route_of, route_question, thresholds as decision_thresholds, Routed, MOVE_PROBABILITY, REFUSE_FLAG, REFUSE_PROBABILITY, ROUTER_FLAG, RULES_SURE};
pub use engine::LayaGate;
pub use feedback_stats::{ledger, RoutingLedger};
pub use fallback::{
    apply_decision_rules, attack_phrase, fallback_decision, fast_chat, goal_is_coding_or_files, harmful_intent, looks_actionable, looks_live_info, needs_web_lookup, REFUSE_REPLY,
};
pub(crate) use fallback::{asks_to_make_pass, IMPERATIVES};
pub use questions::{aqua_gate_questions_json, aqua_route_questions_json, aqua_safety_questions_json};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateRoute {
    Refuse,
    Chat,
    McpAgent,
}

impl GateRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refuse => "refuse",
            Self::Chat => "chat",
            Self::McpAgent => "mcp_agent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "refuse" => Some(Self::Refuse),
            "chat" => Some(Self::Chat),
            "mcp_agent" | "mcp" | "tools" => Some(Self::McpAgent),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceHint {
    Desktop,
    Browser,
    Either,
}

impl SurfaceHint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Browser => "browser",
            Self::Either => "either",
        }
    }

    pub fn agent_prompt_line(self) -> Option<&'static str> {
        match self {
            Self::Desktop => Some("Prefer Windows-MCP tools for this goal (desktop apps / OS UI)."),
            Self::Browser => Some("Prefer Browser-MCP tools for this goal (web pages / Chrome)."),
            Self::Either => None,
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "desktop" => Self::Desktop,
            "browser" => Self::Browser,
            _ => Self::Either,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateDecision {
    pub route: GateRoute,
    pub needs_tools: bool,
    pub jailbreak: f64,
    pub prompt_injection: f64,
    pub harm_severity: f64,
    pub confidence: f64,
    pub surface: SurfaceHint,
    /// `"laya"`, `"fallback"`, `"disabled"`, or `"empty"`.
    pub source: String,
    /// Wall time for this decide() call in milliseconds.
    #[serde(default)]
    pub latency_ms: u64,
}

impl GateDecision {
    pub fn refuse_reply() -> &'static str {
        REFUSE_REPLY
    }

    /// The caller said this is a task for the agent (`force_agent`). What the gate would answer in chat goes to the tools when
    /// there are any to use; a refusal stays a refusal, whatever the caller says.
    pub fn forced_to_agent(mut self, mcp_available: bool) -> Self {
        if self.route == GateRoute::Chat && mcp_available {
            self.route = GateRoute::McpAgent;
            self.needs_tools = true;
            self.source = format!("{}+force_agent", self.source);
        }
        self
    }

    /// Structured log line for tracing / metrics scrapers.
    pub fn log_fields(&self) -> String {
        format!(
            "route={} needs_tools={} surface={} source={} conf={:.3} jailbreak={:.3} injection={:.3} harm={:.3} latency_ms={}",
            self.route.as_str(),
            self.needs_tools,
            self.surface.as_str(),
            self.source,
            self.confidence,
            self.jailbreak,
            self.prompt_injection,
            self.harm_severity,
            self.latency_ms
        )
    }
}

/// Raw scores extracted from a Laya prediction (before policy rules).
#[derive(Debug, Clone)]
pub struct RawGateScores {
    pub route_choice: Option<String>,
    pub needs_tools: f64,
    pub jailbreak: f64,
    pub prompt_injection: f64,
    pub harm_severity: f64,
    pub confidence: f64,
    pub surface_choice: Option<String>,
}
