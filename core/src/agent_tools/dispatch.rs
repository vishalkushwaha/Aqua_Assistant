//! The call of a tool through the host: what the host reported of the last call of a thread, one step of a command, the call of the agent with the data it answered, who is asked, and what a recipe never keeps (move only: this was part of `agent_tools.rs`).

use super::*;

/// What the host reported about the last call this thread made, for the run record: how long it took and how a question (if one was
/// asked) was answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallInfo {
    pub duration_ms: u64,
    /// `approved`, `approved_session`, `approved_always`, `preapproved` (the person allowed this run's project checks ahead of time), `denied` or `timeout`; empty when the policy allowed the call without asking.
    pub approval: String,
}

thread_local! {
    static LAST_CALL: RefCell<Option<CallInfo>> = const { RefCell::new(None) };
}

/// The host's report on the last call made on this thread (once; `None` if there was none since the last take).
pub fn take_last_call() -> Option<CallInfo> {
    LAST_CALL.with(|c| c.borrow_mut().take())
}

thread_local! {
    static LAST_ASKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Was the person asked about the last call this thread made through the host? (Not consumed by reading it: the run record takes the
/// call's report, and the loop asks this separately.)
pub fn last_call_asked() -> bool {
    LAST_ASKED.with(|a| a.get())
}

/// One step of a command (card OA11-02) as a call of the tool host: `Ok` is what it printed, `Err` is why it did not run (refused by the host, by a rule or a hook, not approved, or failed).
/// It is made the way the agent's own calls are (the same flags for a run nobody is there to ask), and written in the audit ledger under `run_id`.
pub fn call_step(host: &ToolHost, tool: &str, arguments: Value, caller: Caller, run_id: &str) -> Result<String, String> {
    let mut ctx = RequestContext::new(caller);
    ctx.agent_run_id = run_id.to_string();
    let alone = nobody_to_ask();
    ctx.no_one_to_ask = alone;
    ctx.hold_changes = alone;
    ctx.unattended = alone;
    let res = host.call_blocking(ToolRequest { tool: tool.to_string(), arguments, context: ctx });
    let text = observation(&res);
    if res.ok {
        Ok(text)
    } else {
        Err(text)
    }
}

/// Run one tool for the agent. Returns the observation text. With `AQUA_DOOR=on` the decision for a browser or desktop tool found some other way (`tools.search`) is the
/// one door's too (card R6-08).
pub fn call(host: &ToolHost, tool: &str, arguments: Value, caller: Caller, run_id: &str) -> String {
    call_detailed(host, tool, arguments, caller, run_id).0
}

/// [`call`], and what the tool answered as data besides the text (`ToolResult::data`: for `fs.read` whether the read was complete and the version of the file, for `fs.patch` the files it
/// changed). Null when the call failed before it ran. The coding loop reads the file tools' answers from here, not from their wording (card OA6-01).
pub fn call_detailed(host: &ToolHost, tool: &str, arguments: Value, caller: Caller, run_id: &str) -> (String, Value) {
    call_with_data(host, tool, arguments, caller, run_id, crate::door_shadow::DoorMode::from_env().applies(), 0)
}

/// [`call`] with the one door given and a time limit in milliseconds (0: the tool's own).
pub(super) fn call_with(host: &ToolHost, tool: &str, arguments: Value, caller: Caller, run_id: &str, door: bool, timeout_ms: u64) -> String {
    call_with_data(host, tool, arguments, caller, run_id, door, timeout_ms).0
}

fn call_with_data(host: &ToolHost, tool: &str, arguments: Value, caller: Caller, run_id: &str, door: bool, timeout_ms: u64) -> (String, Value) {
    // A page may be read only if the person gave its address or a search of this run named it: an address the model composed (with somebody's data
    // in its query, say) is not one of those. The refusal is one the model can act on (search first), not a stop.
    // What lets a spoken request read a page that is not on the allow list is the one thing below: this exact address is a result of this run's own search.
    let mut named_by_a_search = false;
    if tool == "web.read" && !run_id.is_empty() {
        let url = arguments.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if !crate::sources::may_read(run_id, url) {
            return (
                "ERROR [INVALID_ARGUMENTS] web.read: read a page the user named, or one of the results of web.search (use its address exactly). This address is neither: search first.".to_string(),
                Value::Null,
            );
        }
        named_by_a_search = crate::sources::named_by_a_search(run_id, url);
    }
    // What a page is read for, when the agent says (the reader is told).
    let read_for = arguments.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut ctx = RequestContext::new(caller);
    ctx.agent_run_id = run_id.to_string();
    // False on every path where the check above did not positively find this address among the run's search results.
    ctx.named_by_search = named_by_a_search;
    // Nobody is there to answer (a background run, a flow, a practice run): a call that would have to ask is refused at once, in the ledger and to the model as what it is, not
    // recorded as the person's no (a flow's thread used to be refused by the browser's broker, which wrote it down as an answer of the person's), and what the model chose to
    // change on its own waits for the person.
    let alone = nobody_to_ask();
    ctx.no_one_to_ask = alone;
    ctx.hold_changes = alone;
    ctx.unattended = alone;
    ctx.door = door;
    ctx.timeout_ms = timeout_ms;
    ctx.approval_wait_secs = approval_wait_now();
    // A helper agent only looks (card OA11-04): the host refuses its call when it would change something, whatever its list of tools says.
    ctx.agent_scope = agent_scope_now();
    let again = (tool.starts_with("fs.") && !alone).then(|| (arguments.clone(), ctx.clone()));
    // A command that may change files, in a run whose project folder is a trusted one: the folder is looked at once, before the first of them (card OA7-03, ED-11 and CX-11).
    if !run_id.is_empty() {
        checkpoint_before_command(tool, &arguments, run_id);
    }
    // A file tool that changes something: what it changed counts toward the run's limits (card R7-18).
    let counted = (!run_id.is_empty() && crate::autonomy::counts_changes(tool)).then(|| arguments.clone());
    let mut res = host.call_blocking(ToolRequest { tool: tool.to_string(), arguments, context: ctx });
    if let Some((arguments, ctx)) = again {
        if let Some(after) = after_a_refused_folder(host, tool, arguments, &res, ctx, run_id) {
            res = after;
        }
    }
    if let (Some(args), true) = (counted, res.ok) {
        crate::autonomy::note_changed(run_id, tool, &args, &res.data);
    }
    // A file call refused for its folder is written down as a gap (card R7-05); it changes nothing here.
    crate::self_repair::note_refused_file_result(&res, tool, run_id);
    let approval = res.approval.as_ref().map(|a| a.decision.clone()).filter(|d| d != "auto").unwrap_or_default();
    LAST_ASKED.with(|a| a.set(!approval.is_empty()));
    LAST_CALL.with(|c| *c.borrow_mut() = Some(CallInfo { duration_ms: res.duration_ms, approval }));
    let mut text = note_company_result(&res, run_id).or_else(|| note_web_result(&res, run_id, &read_for)).unwrap_or_else(|| observation(&res));
    if res.ok {
        text.push_str(&crate::modules::code_verification::check_hints(tool, &res.data));
    }
    (text, res.data.clone())
}

/// Is nobody there to answer on this thread: a background run, or a thread that a flow, a workflow or a practice run marked unattended?
pub fn nobody_to_ask() -> bool {
    crate::queue::in_background() || aqua_mcp_adapters::browser::is_unattended() || in_helper()
}

/// Is a helper agent (card OA11-04, `agent.task`) running on this thread? Nobody can be asked anything for it, nothing is learned from it, and the host holds its calls to what only looks
/// (`AgentScope::Helper`).
pub fn in_helper() -> bool {
    crate::agents::in_helper()
}

/// What kind of agent makes the calls of this thread (see `RequestContext::agent_scope`).
fn agent_scope_now() -> aqua_tool_host::AgentScope {
    if in_helper() {
        aqua_tool_host::AgentScope::Helper
    } else {
        aqua_tool_host::AgentScope::Main
    }
}

/// Hide what a recipe must never store for a host tool: bodies, commands, queries and secrets.
pub fn redact_args(args: &Value) -> Value {
    let mut a = args.clone();
    if let Some(obj) = a.as_object_mut() {
        for (k, v) in obj.iter_mut() {
            let kl = k.to_ascii_lowercase();
            let sensitive = ["content", "body", "data", "text", "command", "cmd", "sql", "query", "question", "args", "input", "headers", "env", "value", "message", "patch"].contains(&kl.as_str())
                || aqua_tool_host::output::is_secret_key(&kl);
            if sensitive {
                let n = match &*v {
                    Value::String(s) => s.chars().count(),
                    other => other.to_string().chars().count(),
                };
                *v = Value::String(format!("<redacted {n} chars>"));
            }
        }
    }
    a
}
