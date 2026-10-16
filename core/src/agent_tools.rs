//! How the agent uses the tool host (`fs.*`, `term.*`, `git.*`, `web.*`, `ssh.*`, `docker.*`, …).
//!
//! * **Packs and the router.** A small local model cannot choose well among 100 tools, so each
//!   run is shown only the packs its goal mentions (at most two, at most [`MAX_HOST_TOOLS`]
//!   tools). A goal that mentions none sees none, exactly as before.
//! * **Names.** Models write `fs_read_file`, `FS.READ_FILE` or `host.fs.read_file`; those resolve
//!   to the real tool. An unknown name is never guessed at beyond that.
//! * **One door.** Every call goes through `ToolHost::call`, which decides, asks, audits, masks
//!   secrets and tracks the run's taint. This module only translates.
//! * **Runs.** Each agent run gets an id, so taint is per run; cancelling the agent cancels the
//!   host calls of every active run, and ending a run forgets its taint.
//!
//! The code is cut by what it does (move only: no line of code was changed, and every `crate::agent_tools::X` is where it was because this file re-exports each part):
//! `pack_routing`, `tools_shown` and `coding_tools` (which packs and tools a goal is shown, and what their names mean); `observation` and `dispatch` (what a call to the host
//! answers, and the call itself); `door` (the browser and the desktop through the host); `around_call` (the look at a project, the folder a person named); `run_scope` and
//! `run_marks` (the start and the end of a run, and the lists of skills that narrow it: the rest of a run's identity, of what it holds and of what the process knows of the runs is `agent_runs::scope` since card OA13-03, task AQ-12, and this file re-exports it under the names it had). This file keeps the imports that every part reaches through
//! `use super::*`, the three limits every part uses, the tool host of the process (`install`, `host`), the declarations and the re-exports. The tests are the `*_tests` modules and
//! `tests` (the pretend providers they share), each in a file of its own.

use aqua_mcp_adapters::McpTool;
use aqua_tool_host::{Caller, RequestContext, ToolHost, ToolManifest, ToolRequest, ToolResult};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

/// What this process knows about the runs that are going on and the ones that just ended lives in `agent_runs::scope::registry` (card OA13-03, task AQ-12); `around_call` and the start and the end of a run reach it through this name.
use crate::agent_runs::scope::registry;

/// The most host tools one run is shown, on top of the desktop and browser tools.
pub const MAX_HOST_TOOLS: usize = 12;
/// The most packs one run is shown.
pub const MAX_PACKS: usize = 2;
/// Marks tools from the host in `McpTool::source`.
pub const SOURCE: &str = "host";

static HOST: OnceLock<Arc<ToolHost>> = OnceLock::new();

/// Called once at start-up with the host `build_tool_host` made.
pub fn install(host: Arc<ToolHost>) {
    // The run record keeps the host's own digest of each call that changes something (see `ToolHost::set_dispatch_observer`).
    host.set_dispatch_observer(Arc::new(|run, tool, digest| crate::agent_runs::note_dispatch(run, tool, digest)));
    let _ = HOST.set(host);
}

pub fn host() -> Option<&'static Arc<ToolHost>> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(h) = THIS_THREADS_HOST.with(|h| h.get()) {
        return Some(h);
    }
    HOST.get()
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// The host a test gave this thread in place of the process's (see [`with_host`]).
    static THIS_THREADS_HOST: std::cell::Cell<Option<&'static Arc<ToolHost>>> = const { std::cell::Cell::new(None) };
}

/// Runs `f` with `host` as the tool host of **this thread** (card OA3-01, AQ-04). The process has one host, installed once at start-up (`install`), and a test that wants the
/// agent loop's dispatch (`browser_agent::call_named_tool`) to reach a host of its own, with the providers, hooks and ledger it chose, could only hope that no other test
/// had installed theirs first. The host is kept for the life of the process (a `&'static` is what [`host`] gives out), which for a test is a few kilobytes. It is compiled for this crate's own tests and for the `test-support` feature (the scenario tests of `core/tests/` give each scenario a host of its own through `test_support::agent`, card OA2-04); a release build has neither.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn with_host<R>(host: Arc<ToolHost>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<&'static Arc<ToolHost>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            THIS_THREADS_HOST.with(|h| h.set(self.0));
        }
    }
    let kept: &'static Arc<ToolHost> = Box::leak(Box::new(host));
    let _restore = Restore(THIS_THREADS_HOST.with(|h| h.replace(Some(kept))));
    f()
}

/// Which packs of tools a goal calls for.
mod pack_routing;
pub use self::pack_routing::*;

/// Which tools a run is shown for its goal, the names a model writes for them, and what a tool can do.
mod tools_shown;
pub use self::tools_shown::*;

/// The coding set: what a coding goal is shown, and the one more pack `AQUA_CODING_PACKS` allows.
mod coding_tools;
pub use self::coding_tools::*;

/// What the agent reads of a tool's result.
mod observation;
pub use self::observation::*;

/// The call of a tool through the host.
mod dispatch;
pub use self::dispatch::*;

/// The one door for the browser and the desktop, and the comparison with the old routes.
mod door;
pub use self::door::*;

/// The look at a project before a command, and the question about a folder the person named.
mod around_call;
pub use self::around_call::*;

/// The start and the end of a run, and the hooks at its start (the rest of a run's identity and life is in `agent_runs::scope`).
mod run_scope;
pub use self::run_scope::*;

/// The lists of the skills that narrowed a run, and how they go on to the run that continues it (the rest of what a run holds is in `agent_runs::scope`).
mod run_marks;
pub(crate) use self::run_marks::*;

/// A run's identity and life on its thread, what a run holds and what narrows its calls, and what the process knows of the runs that are going on moved to `agent_runs::scope` (card OA13-03, task AQ-12, with no change to a line of their code);
/// every public name they had here is re-exported, so that every old path works (`forget_ended_runs`, a helper of the tests of this crate, is only at its new place).
pub use crate::agent_runs::scope::{
    approval_wait_now, bind_agent, cancel_active, current_run, ended_run_marks, foreground_run_active, foreground_run_id, inherit_run_marks, inherit_run_marks_in, is_active, limit_stop, limit_stop_on, mark_current_run_private, new_run_id,
    note_found, options_of, origin_of_run, run_is_tainted, run_marks, run_that_just_ended, runs_in_flight, runs_start_tainted, skill_refusal, skill_refusal_on, take_found, taint_current_run, with_approval_wait, with_planned_run_id,
    with_run_options, RunOptions, RunScope, TaintedStart,
};

/// The pretend providers and hosts the tests share (`crate::agent_tools::tests::wide_host` is used by the tests of the loop).
#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod tools_shown_tests;

#[cfg(test)]
mod dispatch_tests;

#[cfg(test)]
mod pack_routing_tests;

#[cfg(test)]
mod coding_tools_tests;

#[cfg(test)]
mod observation_tests;

#[cfg(test)]
mod door_tests;

#[cfg(test)]
mod around_call_tests;
