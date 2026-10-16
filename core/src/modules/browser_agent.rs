//! Llama.cpp ↔ multi-MCP agent loop (Browser MCP + Windows-MCP).
//!
//! MCP mutexes are held only during short start/list/call operations — never during LLM waits.
//! Successful tool sequences are stored in SolutionsDb; failures update RL + reflection memory.
//!
//! The code is cut by what it does (card OA3-05, move only: no logic changed, and every `crate::modules::browser_agent::X` is where it was because this file
//! re-exports each part): `entry` (the ways into the loop) and `agent_loop` (the loop itself); `prompt`, `history` and `coding_record` (what the planner is shown);
//! `reply` (what the planner said); `guards` (the checks around a call and a run) and `final_gate` (the checks on a final answer); `call` (the one dispatch),
//! `tools` and `fast_path` (the tools on offer, the replay of a recipe); `scripts` (the goal-specific scripts); `recovery` (a failed request for the next step);
//! `state` (what a run keeps on its thread) and `project` (the folder of the project). This file keeps the public types, the limits on rounds and the declarations.
//! The tests are the `*_tests` modules and `tests`, each in a file of its own.

use anyhow::{bail, Context, Result};
use aqua_mcp_adapters::browser::{BackendKind, BrowserBackend};
use aqua_mcp_adapters::{
    filter_windows_tools, observation_indicates_extension_offline, open_web_url, tools_prompt_block,
    McpTool, StdioMcpClient, WindowsCaller, WindowsGuard,
};
use crate::laya_validator::GateDecision;
use crate::memory::{
    AgentLearning, LearnedRecipe, LearnedToolCall,
};
use crate::modules::llm_router::LlamaRouterEngine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// Recipes version 2: the replay of a learned recipe with placeholders and postconditions (card R2-13, `AQUA_RECIPES_V2=on`).
mod recipe_replay;

/// The coding session in the loop: the record of the project's earlier work told to the run, the notes it writes, and what it leaves behind when it ends (card OA10-04, `AQUA_CODING_SESSIONS=on`).
mod session_run;

/// Helpers a run starts with `agent.task`, and the agent a run is told to be (card OA11-04, `AQUA_AGENTS=on`).
mod child;

/// What a run keeps on the thread that runs it: one `RunContext` in the one `thread_local!` of the loop, with the functions that set and read it (card OA3-05; card OA13-03).
mod state;
pub use self::state::*;

/// The folder of the project a goal is about, and the project notes for the planner (card OA3-05).
mod project;
pub(crate) use self::project::*;

/// The checks around a call and around a run (card OA3-05).
mod guards;
pub use self::guards::*;

/// The checks on a final answer before a run may end with it (card OA3-05).
mod final_gate;
pub(crate) use self::final_gate::*;

/// What the loop does when the request for the next step fails (card OA3-05).
mod recovery;
use self::recovery::*;

/// The goal-specific scripts of the loop: a site, an e-mail, a search, a plain app, the headlines of a news page (card OA3-05).
mod scripts;
pub(crate) use self::scripts::*;

/// The planner's prompt: the rules, the tools it is shown, the goal and what has happened, and the budget that fits them in the window (card OA3-05).
mod prompt;
pub(crate) use self::prompt::*;

/// What the model is shown of what has happened: a result cut, cleaned and fenced as data, the earlier results as lines (card OA3-05).
mod history;
pub(crate) use self::history::*;

/// The record of a coding run as the words a prompt ends with, and what a coding run that stops tells the person (card OA3-05).
mod coding_record;
use self::coding_record::*;

/// What the planner said and what a run ends with: the reader of its reply, the answer with its citations, the words of a run that was not answered (card OA3-05).
mod reply;
use self::reply::*;

/// The tools the loop offers and how they are reached (card OA3-05).
mod tools;
pub(crate) use self::tools::*;

/// A recipe learned from an earlier run replayed with no planning round, and what is kept of a run as a recipe (card OA3-05).
mod fast_path;
pub use self::fast_path::*;

/// The one dispatch of the loop: every call the model makes comes through `call_named_tool` (card OA3-05).
mod call;
pub(crate) use self::call::*;

/// The ways into the loop (card OA3-05).
mod entry;
pub use self::entry::*;

/// The loop itself: `run_mcp_agent_inner`, moved whole (card OA3-05).
mod agent_loop;
pub(crate) use self::agent_loop::*;

/// The most planning rounds one agent run takes. The API, workflows and flows all limit the value they accept to this, so a
/// setting above it is refused or shown as the limit instead of quietly running fewer rounds than it says.
pub const AGENT_MAX_ROUNDS: u32 = 12;
/// A coding goal reads, edits and re-runs a check. Twelve rounds is the cap for every other goal.
pub const CODING_MAX_ROUNDS: u32 = 24;

/// The round cap for this goal. Coding work uses [`CODING_MAX_ROUNDS`]; everything else stays at [`AGENT_MAX_ROUNDS`]. Inside a run this is the run's profile (card OA4-01); before one it is
/// what the words of the goal make of it.
pub fn max_rounds_for_goal(goal: &str) -> u32 {
    if crate::modules::run_profile::coding(goal) { CODING_MAX_ROUNDS } else { AGENT_MAX_ROUNDS }
}

/// [`max_rounds_for_goal`] for a request that may say what the task is (the `profile` field): what it says wins, as it does for the run itself.
pub fn max_rounds_for_request(goal: &str, requested: Option<crate::modules::run_profile::RunProfile>) -> u32 {
    if crate::modules::run_profile::profile_for(goal, requested) == crate::modules::run_profile::RunProfile::Coding { CODING_MAX_ROUNDS } else { AGENT_MAX_ROUNDS }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStep {
    pub step: u32,
    pub kind: String,
    pub detail: Value,
}

/// Build a monitor-friendly phase event (streamed live to the desktop UI).
pub fn phase_event(
    step: u32,
    id: &str,
    title: impl Into<String>,
    status: &str,
    message: impl Into<String>,
    next: Option<&str>,
    progress_cur: Option<u32>,
    progress_total: Option<u32>,
) -> AgentStep {
    let mut detail = json!({
        "id": id,
        "phase": id,
        "title": title.into(),
        "status": status,
        "message": message.into(),
    });
    if let Some(n) = next {
        detail["next"] = json!(n);
    }
    if let (Some(c), Some(t)) = (progress_cur, progress_total) {
        detail["progress"] = json!({ "current": c, "total": t });
    }
    AgentStep {
        step,
        kind: "phase".into(),
        detail,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpAgentResult {
    pub final_answer: String,
    pub steps: Vec<AgentStep>,
    pub tool_rounds: u32,
    pub stopped_reason: String,
    /// Present when Laya gated this request (refuse/chat skip, or mcp entry).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub gate: Option<GateDecision>,
    /// The id the run is recorded under (`GET /api/v1/runs/{id}`). Absent when the run was not recorded.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub run_id: Option<String>,
    /// The planner asked the person something instead of guessing (`stopped_reason` is `waiting_user`): the question, and the answers it
    /// offered (possibly none). The question is also `final_answer`, so a caller that knows nothing of this says it as it would any answer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub question: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub choices: Vec<String>,
    /// The web pages the answer cites, as `{id, url, title, date}` (the `[n]` in the answer is the id). Empty when nothing was read on the web.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub sources: Vec<Value>,
}

impl McpAgentResult {
    pub fn with_gate(mut self, gate: GateDecision) -> Self {
        self.gate = Some(gate);
        self
    }
}

pub type BrowserAgentResult = McpAgentResult;

/// Shared handle to the policy-guarded browser layer (managed or extension backend).
type BrowserHandle = Arc<BrowserBackend>;

/// The coding loop run end to end with no model: a scripted planner over the real tool host and the fixtures of `core/tests/fixtures/coding/` (task OA2-03, WF-02). It is declared here, below
/// all the code that is not a test and not above it: `docs_gen`'s check that every claimed symbol has a caller reads a file only up to its first `#[cfg(test)]` module, and a declaration at the
/// top of this file made every symbol of the file look unused (card OA5-01 moved it, with no other change).
#[cfg(test)]
mod coding_loop_tests;

/// Tests of the prompt that fits the window (card OA4-02); in a file of their own, `browser_agent/prompt_budget_tests.rs`.
#[cfg(test)]
mod prompt_budget_tests;

/// The `Stop` hooks are told of a run that ends in an error (card OA11-01); in a file of their own, `browser_agent/stop_hook_tests.rs`.
#[cfg(test)]
mod stop_hook_tests;

/// Property-style tests of the reader of the planner's reply (card OA13-01); in a file of their own, `browser_agent/parse_properties_tests.rs`.
#[cfg(test)]
mod parse_properties_tests;

/// A native tool call is read as the same action the JSON reply gives (card OA9-06); in a file of their own, `browser_agent/native_mode_tests.rs`.
#[cfg(test)]
mod native_mode_tests;

/// Tests of the loop in a file of their own, `browser_agent/tests.rs` (card OA3-05).
#[cfg(test)]
mod tests;

/// Tests of the loop in a file of their own, `browser_agent/planner_budget_tests.rs` (card OA3-05).
#[cfg(test)]
mod planner_budget_tests;

/// Tests of the loop in a file of their own, `browser_agent/evidence_check_tests.rs` (card OA3-05).
#[cfg(test)]
mod evidence_check_tests;

/// Tests of the loop in a file of their own, `browser_agent/prompt_cache_tests.rs` (card OA3-05).
#[cfg(test)]
mod prompt_cache_tests;

/// Tests of the loop in a file of their own, `browser_agent/snapshot_metadata_args_tests.rs` (card OA3-05).
#[cfg(test)]
mod snapshot_metadata_args_tests;

/// Card OA5-01 (tasks AG-01, TL-02, WF-11): every tool a prompt names is one the reply can name.
#[cfg(test)]
mod reachability_tests;
