//! What a run holds and what narrows its calls: the taint and the private mark, the list of a skill or of an agent, the limits of the autonomy setting, and how the marks go on to the run that continues one that asked a question (move only: this was part of `agent_tools/run_marks.rs`, card OA13-03, task AQ-12). What stays in `agent_tools/run_marks.rs` is the part that reads the lists of the skills (`restrictions_from_record`, `inherit_marks_on`): it names `skills`, which `agent_runs` does not name.

use super::*;

/// The run `run_id` is an agent of the person's choosing from now on (card OA11-04): the host refuses every tool its list does not name (a second list narrows further, as a loaded skill's does:
/// it can only take tools away), and the lists of tools the run is shown, the words of its prompt and its limits are the agent's. Nothing is widened.
pub fn bind_agent(host: Option<&ToolHost>, run_id: &str, used: crate::agents::AgentUse) -> Result<(), String> {
    // The host's list first: if the rest cannot be bound, the run is still held to the tools of the agent, and the caller does not let it go on as that agent.
    if let Some(h) = host {
        h.restrict_run(run_id, used.allowed_tools.clone());
    }
    crate::agents::bind(run_id, used)
}

/// The current run has just read a web page or a desktop window: from now on it is tainted, so anything that
/// changes something needs a fresh answer from the person, and earlier "approve for this session" grants stop
/// applying. Reads that go through the tool host do this themselves; the browser and desktop routes call this.
pub fn taint_current_run() {
    if let Some(host) = host() {
        host.mark_tainted(&current_run());
        crate::agent_runs::note_marks();
    }
}

/// The current run has read the screen or the clipboard: what it holds is the person's, so what it may send out is held to the stricter rules
/// (see `ProviderOutput::private`). Reads that go through the tool host mark this themselves; the desktop route calls this.
pub fn mark_current_run_private() {
    if let Some(host) = host() {
        host.mark_private(&current_run());
        crate::agent_runs::note_marks();
    }
}

/// What the run holds as the host sees it now: (read untrusted text, holds private data). `None` without a host.
pub fn run_marks(run_id: &str) -> Option<(bool, bool)> {
    let host = host()?;
    Some((host.is_tainted(run_id), host.is_private(run_id)))
}

/// Has this run read a web page or a window (or any other untrusted text)?
pub fn run_is_tainted() -> bool {
    host().is_some_and(|h| h.is_tainted(&current_run()))
}

/// The refusal for a call that a loaded skill's list does not name, in the words the host uses, or `None` (card R6-16). The host applies the list to the calls that go
/// through it; the browser, the desktop and the search of the computer do not, so they ask here. A list may name a tool as the model writes it (`browser_click`) or as
/// the host does (`browser.click`): a call is allowed when either is named.
pub fn skill_refusal_on(host: &ToolHost, run: &str, tool: &str) -> Option<String> {
    let mut names = vec![tool.to_string()];
    if let Some((prefix, rest)) = tool.split_once('_') {
        if matches!(prefix, "browser" | "local") {
            names.push(format!("{prefix}.{rest}"));
        }
    }
    let refusals: Vec<Option<String>> = names.iter().map(|n| host.restriction_refusal(run, n)).collect();
    if refusals.iter().any(Option::is_none) {
        return None;
    }
    refusals.into_iter().next().flatten().map(|why| format!("ERROR [PERMISSION_DENIED] {tool}: {why}"))
}

/// [`skill_refusal_on`] for the run on this thread.
pub fn skill_refusal(tool: &str) -> Option<String> {
    skill_refusal_on(host()?, &current_run(), tool)
}

/// What the run reads in place of a call when it has reached a limit of the autonomy setting (card R7-18), or `None` when the call may go on. It counts the call when it may. The
/// limits are those of `autonomy` (calls, seconds, files and bytes changed); `tool` is the name as the model wrote it, and `arguments` tell which files a changing call names.
pub fn limit_stop_on(autonomy: &crate::autonomy::Autonomy, run: &str, tool: &str, arguments: &Value) -> Option<String> {
    autonomy.check_before_call(run, tool, arguments).err().map(|stop| stop.observation(tool))
}

/// [`limit_stop_on`] for the run on this thread, with the setting and counters of this process. Asked in one place, where the agent loop dispatches a call, so that it covers the
/// host's tools, the browser, the desktop and the local tools alike.
pub fn limit_stop(tool: &str, arguments: &Value) -> Option<String> {
    let run = current_run();
    if run.is_empty() {
        return None;
    }
    limit_stop_on(&crate::autonomy::shared(), &run, tool, arguments)
}

/// What a new process would not know: the runs that just ended (tests of a run that goes on from one an earlier process stopped).
#[cfg(test)]
pub(crate) fn forget_ended_runs() {
    registry().forget_ended();
}

/// The current run goes on from `parent` (a run that ended with a question): see [`inherit_marks_on`].
pub fn inherit_run_marks(parent: &str, had_history: bool) {
    inherit_run_marks_in(parent, had_history, crate::agent_runs::store().map(|s| &**s));
}

/// [`inherit_run_marks`] with the run record to look in for what a run that stopped held (the core's own record, or a test's).
pub fn inherit_run_marks_in(parent: &str, had_history: bool, store: Option<&crate::agent_runs::AgentRunStore>) {
    if let Some(host) = host() {
        inherit_marks_on(host, &current_run(), parent, had_history, store);
    }
}
