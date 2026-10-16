//! The scope of an agent run (card OA13-03, task AQ-12 of the OpenCode audit): what a run is, what it holds, and what the process knows of the runs that are going on and the ones that just ended.
//!
//! * `run_scope`: a run's identity and life on its thread (`RunScope`: the id, the span of its log lines), the id and the wait for an answer a run was told, the options a command gave it (`RunOptions`), the runs that start as having read outside text (`runs_start_tainted`), and the queries of what the process knows of the runs (`runs_in_flight`, `foreground_run_id`, `is_active`, `cancel_active`).
//! * `run_marks`: what a run holds (the taint and the private mark: `taint_current_run`, `run_marks`), what narrows its calls (the list of a skill or of an agent: `skill_refusal_on`, `bind_agent`; the limits of the autonomy setting: `limit_stop`) and how the marks go on to the run that continues one that asked a question (`inherit_run_marks`).
//! * `registry`: `RunRegistry`, the tables of the runs that are going on and of the ones that just ended.
//!
//! This is the code that was `agent_tools/run_scope.rs`, `agent_tools/run_marks.rs` and `agent_tools/registry.rs`, moved with no change to a line of it (the visibility of some items is widened where `agent_tools` now needs them, and `use` lines are the new ones). Five things stay in `agent_tools`, because
//! each names a module that `agent_runs` does not name, and a new edge between two modules is not added: the start of a run (`RunScope::begin` asks `queue` whether the run is in the background), the end of it (`Drop for RunScope` lets `project_context` go of the run), the hooks at its start
//! (`hooks_at_run_start` asks both), and the part that reads the lists of the skills (`restrictions_from_record` and `inherit_marks_on` name `skills`); `inherit_run_marks_in` calls the last one through the one edge that exists, `agent_runs -> agent_tools`. `agent_tools` re-exports every name that moved, so the old paths work.

use crate::agent_tools::{host, inherit_marks_on};
use aqua_tool_host::ToolHost;
use serde_json::Value;
use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

/// What this process knows about the runs that are going on and the ones that just ended.
mod registry;
pub(crate) use self::registry::registry;
#[cfg(test)]
use self::registry::ENDED_KEEP;
use self::registry::EndedRun;

/// A run's identity and life on its thread, and what the process knows of the runs that are going on.
mod run_scope;
pub use self::run_scope::*;

/// What a run holds and what narrows its calls, and how both go on to the run that continues it.
mod run_marks;
pub use self::run_marks::*;

#[cfg(test)]
mod run_marks_tests;

#[cfg(test)]
mod run_scope_tests;
