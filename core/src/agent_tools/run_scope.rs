//! The start and the end of a run, and the hooks at its start (card OA13-03, task AQ-12: the rest of what was here, the id, the options, the registry of the runs, is `agent_runs::scope`; every name is still reached from `agent_tools` as before). They stay because `RunScope::begin` asks `queue` whether the run is in the background, `Drop for RunScope` lets `project_context` go of the run, and `hooks_at_run_start` asks both: `agent_runs` may not name either module, and a new edge is not added.

use super::*;
use crate::agent_runs::scope::{note_ended, LAST_ENDED, PENDING_OPTIONS, PLANNED_RUN_ID, RUN_ID, STARTS_TAINTED};

/// What the person's hooks say about a run that is about to start (card OA11-01, task EX-04). `UserPromptSubmit` is given the start of the words the run was asked to do (500 characters, masked)
/// and may refuse the run, or make the person say yes first; `RunStart` is told that it began. `Some(words)` is what to answer instead of running: the refusal, or the person's no, is in the
/// ledger. `None`: go on. Nothing is built or asked when no hook asks for either event.
pub fn hooks_at_run_start(run_id: &str, goal: &str, route: &str, caller: &str) -> Option<String> {
    use aqua_tool_host::{HookAnswer, HookEvent};
    let host = host()?;
    if !host.hooks_want("UserPromptSubmit") && !host.hooks_want("RunStart") {
        return None;
    }
    let head: String = host.redact_text(goal).chars().take(500).collect();
    let project = crate::project_context::project_dir_of_run(run_id).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let mut ctx = RequestContext::new(Caller::Agent);
    ctx.agent_run_id = run_id.to_string();
    ctx.unattended = aqua_mcp_adapters::browser::is_unattended();
    ctx.no_one_to_ask = crate::queue::in_background();
    let event = |name: &'static str| HookEvent { event: name, run_id: run_id.to_string(), project: project.clone(), caller: caller.to_string(), route: route.to_string(), goal_head: head.clone(), ..Default::default() };
    match host.tell_hooks(&event("UserPromptSubmit")) {
        HookAnswer::Deny(why) => {
            host.record_hook_refusal(&ctx, "hook.user_prompt", &head, &why);
            return Some(format!("A hook of yours refused this request before it started: {}", host.redact_text(&why)));
        }
        HookAnswer::Ask(why) => {
            if !host.ask_about_hook(&ctx, "hook.user_prompt", &format!("Start this request? {head}"), &why) {
                return Some(format!("A hook of yours asked about this request before it started, and it was not approved: {}", host.redact_text(&why)));
            }
        }
        HookAnswer::Pass => {}
    }
    host.tell_hooks(&event("RunStart"));
    None
}

impl RunScope {
    pub fn begin(host: Option<Arc<ToolHost>>) -> Self {
        // The process start time is in the id so it stays unique across restarts: the id is also the key of the run record. A run whose id was told to its client before it began has that one.
        let id = PLANNED_RUN_ID.with(|p| p.borrow_mut().take()).unwrap_or_else(new_run_id);
        if let Some(h) = &host {
            if STARTS_TAINTED.with(std::cell::Cell::get) {
                h.mark_tainted(&id);
            }
        }
        registry().begin(&id, crate::agent_runs::current_origin(), crate::queue::in_background(), aqua_mcp_adapters::browser::is_unattended());
        // What a command said about this run (card OA11-02): the host refuses every tool outside the list, and the options stay for the lists of tools the run is shown.
        if let Some(options) = PENDING_OPTIONS.with(|p| p.borrow_mut().take()).filter(|o| *o != RunOptions::default()) {
            if let Some(h) = &host {
                if options.tainted {
                    h.mark_tainted(&id);
                }
                h.restrict_run(&id, options.allowed_tools.clone());
            }
            registry().set_options(&id, options);
        }
        let previous = RUN_ID.with(|r| r.replace(id.clone()));
        let span = tracing::info_span!("run", run_id = %id, route = tracing::field::Empty, caller = tracing::field::Empty);
        let _entered = span.clone().entered();
        Self { id, previous, host, span, _entered }
    }
}

impl Drop for RunScope {
    fn drop(&mut self) {
        LAST_ENDED.with(|l| *l.borrow_mut() = self.id.clone());
        registry().end(&self.id);
        // The limits' counters of the run (card R7-18) go with it.
        crate::autonomy::forget_run(&self.id);
        // So do the files the run read and changed and the checks it asked for (card OA6-01, AG-08): the tables are as long as the runs that are going on, not as the runs there have been.
        crate::modules::coding_session::forget_run(&self.id);
        crate::modules::coding_runner::forget_run(&self.id);
        // The project folder is looked at again when the run had a checkpoint taken (card OA7-03): five seconds at the most, and only for a run that has one.
        finish_checkpoint(&self.id);
        // The project the run worked in, and its pack, are let go with it (card OA11-01); so is the agent it was (card OA11-04) and how many helpers it had started.
        crate::project_context::end_run(&self.id);
        crate::agents::end_run(&self.id);
        if let Some(h) = &self.host {
            note_ended(h, &self.id);
            h.end_run(&self.id);
        }
        // The run is done with the screen: its lease on the desktop is let go (card R6-34), so that the next session does not wait for the idle limit.
        aqua_mcp_adapters::windows_mcp::session::global().release(&self.id);
        RUN_ID.with(|r| *r.borrow_mut() = std::mem::take(&mut self.previous));
    }
}
