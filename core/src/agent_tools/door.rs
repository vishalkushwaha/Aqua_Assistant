//! The one door for the browser and the desktop: their calls through the host with `AQUA_DOOR=on`, and the comparison of what the host would have decided beside what the old routes did (move only: this was part of `agent_tools.rs`).

use super::*;

/// How long a browser or desktop call may take through the host: what the old routes allowed (the desktop's server waits up to two minutes for an answer), and the longest the
/// host allows any of them. The tools' own defaults (30 and 60 seconds) would cut a `WaitFor` of 90 seconds short.
const DOOR_TIMEOUT_MS: u64 = 120_000;

/// The host's name for a browser tool as the model writes it (`browser_click`), or for a desktop tool (`Click`, in any case: the desktop guard reads them that way).
fn door_tool_name(model_name: &str) -> Option<&'static str> {
    aqua_mcp_adapters::browser::provider::host_name_for_wire(model_name).or_else(|| aqua_mcp_adapters::windows_mcp::provider::host_name_for_model_name(model_name))
}

/// With the one door on (`AQUA_DOOR=on`) a browser or desktop call goes through the tool host (card R6-08): the door's decision is applied (never looser than the ordinary one), the
/// person's hooks and a loaded skill's list apply, the refusal limit and the signed ledger apply, and the host asks, masks and records. `None` when the door is not on, when there
/// is no host, or when the name is not a browser or desktop tool: the caller then takes the route it has always taken. The text is what the agent reads, as for any host tool.
pub fn door_call(mode: crate::door_shadow::DoorMode, host: Option<&ToolHost>, model_name: &str, arguments: Value, caller: Caller, run_id: &str) -> Option<String> {
    if !mode.applies() {
        return None;
    }
    let host = host?;
    let tool = door_tool_name(model_name)?;
    let text = call_with(host, tool, arguments, caller, run_id, true, DOOR_TIMEOUT_MS);
    // What the call read (a page, a window) marked the run; the run record keeps the marks as they change.
    crate::agent_runs::note_marks();
    Some(text)
}

/// A browser or desktop call made straight to the API by the person's own app (`/api/v1/browser/call`, `/windows-mcp/call`, `/browser-mcp/call`), with no run behind it (card R6-08).
/// With `AQUA_DOOR=on` it goes through the tool host as the app: the person's hooks, a loaded skill's list and the signed ledger apply to it, and the host's question replaces the
/// route's own. `None`: the route makes the call itself, as it always has. `mode` is `DoorMode::from_env()`, read by the route before it hands the call to a worker thread
/// (card OA3-01: a test pins it for the thread of the request, and the worker would not see that).
pub fn door_route_call(mode: crate::door_shadow::DoorMode, host: &ToolHost, model_name: &str, arguments: Value, caller: Caller) -> Option<String> {
    door_call(mode, Some(host), model_name, arguments, caller, "")
}

/// What the tool host says about a browser or desktop call that does not go through it yet, taken before the call (the call changes what
/// the run holds). `decision` is `None` when the host could not say (the arguments the old route accepts are not the host's).
pub struct DoorPreview {
    pub(super) tool: &'static str,
    pub(super) decision: Option<aqua_tool_host::Decision>,
    /// What the one door would apply: the stricter of the two rules, with a send held to what the run has read and holds (`ToolHost::preview`). Never looser than `decision`.
    pub(super) door: Option<aqua_tool_host::Decision>,
    /// The questions the old routes had put on this thread when the call began, to tell whether the old route asked about this one (card R6-08).
    pub(super) questions_before: u64,
}

/// The comparison of the one-door card runs unless `AQUA_DOOR=off` (or the old name, `AQUA_ONE_DOOR=off`): see [`crate::door_shadow::DoorMode`]. It only asks the host what it
/// would decide: nothing runs, nobody is asked and nothing is written down about the call.
fn one_door_shadow_on() -> bool {
    crate::door_shadow::DoorMode::from_env().compares()
}

pub fn one_door_preview(wire: &str, arguments: &Value, caller: Caller) -> Option<DoorPreview> {
    if !one_door_shadow_on() {
        return None;
    }
    let host = host()?;
    let tool = aqua_mcp_adapters::browser::provider::host_name_for_wire(wire).or_else(|| aqua_mcp_adapters::windows_mcp::provider::host_name_for_wire(wire))?;
    let mut ctx = RequestContext::new(caller);
    let run = current_run();
    ctx.agent_run_id = run.clone();
    let preview = host.preview(&ToolRequest { tool: tool.to_string(), arguments: arguments.clone(), context: ctx }).ok();
    // Who is calling, in which run and what kind of call it is (the host's classes), for the evidence report (card OA8-05): words only, never the arguments.
    let classes = preview.as_ref().map(|p| p.classes.iter().map(|c| format!("{c:?}").to_ascii_lowercase()).collect::<Vec<_>>().join("+")).unwrap_or_default();
    crate::door_shadow::note_call_facts(caller.as_str(), &run, &classes);
    Some(DoorPreview { tool, decision: preview.as_ref().map(|p| p.decision), door: preview.as_ref().map(|p| p.door), questions_before: crate::door_shadow::take_question_baseline() })
}

/// What the old route did, as far as its text says: it ran, it failed, or a rule refused it.
pub fn legacy_outcome(observation: &str) -> &'static str {
    let t = observation.trim_start();
    if !t.starts_with("ERROR") {
        "ran"
    } else if t.starts_with("ERROR [POLICY") || t.contains("[APPROVAL_DENIED]") || t.contains("[PERMISSION_DENIED]") || t.contains("[SECRET_BLOCKED]") {
        "refused"
    } else {
        "failed"
    }
}

fn decision_word(d: Option<aqua_tool_host::Decision>) -> &'static str {
    use aqua_tool_host::Decision::*;
    match d {
        Some(Auto) => "auto",
        Some(Ask) => "ask",
        Some(AlwaysAsk) => "always_ask",
        Some(Deny) => "deny",
        None => "no_preview",
    }
}

/// Counts what the host would have decided beside what the old route did (`aqua_one_door_total`), and says so in the log where the host would
/// refuse something that ran, or let through something that was refused. Where the host says "ask", the old route may have asked on its own, which
/// its text does not tell, so that is counted and not flagged.
pub fn one_door_compare(preview: Option<DoorPreview>, observation: &str) {
    let Some(p) = preview else { return };
    let host_says = decision_word(p.decision);
    let legacy = legacy_outcome(observation);
    crate::metrics::inc("aqua_one_door_total", &[("tool", p.tool), ("host", host_says), ("legacy", legacy)]);
    if (host_says == "deny" && legacy == "ran") || (host_says == "auto" && legacy == "refused") {
        tracing::info!("one-door shadow: {} would be {host_says} through the tool host, and the old route {legacy}", p.tool);
    }
    // And whether the old route put a question to the person, which its text does not tell: the door may be switched on only if it was never looser than the old route
    // (`aqua_one_door_looser_total` stays at nothing, and `data/door_shadow.json` shows where it did not).
    let stance = crate::door_shadow::stance_of(observation, crate::door_shadow::asked_since(p.questions_before));
    let door = p.door.or(p.decision);
    let looser = crate::door_shadow::is_looser(door, stance);
    let door_says = decision_word(door);
    // Taken whether or not it is used, so that it is never left for another call.
    let facts = crate::door_shadow::take_call_facts();
    if door.is_some() {
        crate::door_shadow::record_with(p.tool, host_says, door_says, stance, looser, &facts);
    }
    if looser {
        crate::metrics::describe("aqua_one_door_looser_total", "Browser and desktop calls where the one door would have decided looser than the old route did (it must stay at nothing before the door is switched on).");
        crate::metrics::inc("aqua_one_door_looser_total", &[("tool", p.tool), ("door", door_says), ("legacy", stance.word())]);
        tracing::info!("one-door shadow: {} would be {door_says} through the one door, and the old route {}: the door is looser here", p.tool, stance.word());
    }
}
