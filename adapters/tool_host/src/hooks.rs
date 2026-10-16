//! Hooks: the person's own checks around a tool call. A hook can only **tighten**: it can make a call ask or refuse it, never let through
//! what the policy asks about or refuses, and after a call it can only mask text or add a short note. The host calls them; what they are
//! (programs run from `data/hooks.json`, say) is not its business.

use serde::Serialize;
use serde_json::Value;

/// Does a tool name fit a pattern? Exact (`term.run`), a prefix with a star (`fs.*`, `term.r*`) or `*`.
pub fn tool_pattern_matches(pattern: &str, tool: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => tool.starts_with(prefix),
        None => pattern == tool,
    }
}

/// What a hook is told about a call that is about to run (arguments masked and shrunk like the audit line's).
#[derive(Debug, Clone, Default, Serialize)]
pub struct HookCall {
    pub event: &'static str,
    pub tool: String,
    pub args: Value,
    /// `READ`, `WRITE`, ...
    pub classes: Vec<String>,
    /// What the call is about (a path, an address, a program), masked. For an event after the call it is the target the call had, not an empty one.
    pub target: String,
    /// What the host decided for the call: `auto`, `ask`, `always_ask`. For an event after the call it is what was decided then (what a hook before it said is in it), not `auto`.
    pub decision: &'static str,
    pub caller: String,
    pub run_id: String,
    /// The folder of the project the run works in (card OA11-01; empty when the run has none or the host was not told how to find it).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub project: String,
}

/// What the person's hooks are told about something that is not a call about to run: a call that failed or was refused, a file a call changed, the start of a run, the words a person typed,
/// the end of a run. A hook that is run for one of these can only look (see [`Hooks::on_event`]).
#[derive(Debug, Clone, Default, Serialize)]
pub struct HookEvent {
    /// `PostToolUseFailure`, `FileChanged`, `UserPromptSubmit`, `RunStart` or `Stop`.
    pub event: &'static str,
    pub run_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub project: String,
    pub caller: String,
    /// The tool, for a call that failed or a file that a call changed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tool: String,
    /// The path a call changed, or what a failed call was about: masked.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// How the call ended in the host's words (`denied_by_policy`, `denied_by_hook`, `approved`, `auto`, ...).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// One line, masked and cut: why a call failed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// The start of the goal of a run, masked and cut at 500 characters (`UserPromptSubmit`, `RunStart`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub goal_head: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub route: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stopped_reason: String,
}

/// What a hook says about a call before it runs. `Pass` changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAnswer {
    Pass,
    /// The call asks the person, whatever the policy decided (it cannot lower anything).
    Ask(String),
    /// The call does not run.
    Deny(String),
}

/// What a hook says about a result: text to mask, and a short note for the model (shown as data).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostAnswer {
    pub redact: Vec<String>,
    pub note: Option<String>,
}

/// The methods block (a hook may run a program), so the host calls them from a blocking thread.
pub trait Hooks: Send + Sync {
    fn pre_tool(&self, call: &HookCall) -> HookAnswer;
    fn post_tool(&self, call: &HookCall, ok: bool, text: &str) -> PostAnswer;

    /// Something that is not a call about to run happened (see [`HookEvent`]). The answer matters only for `UserPromptSubmit`: `Deny` stops the run before it starts and `Ask` makes
    /// the person say yes first. For every other event it is ignored: those hooks can only look. Nothing here lets anything through: there is no answer that allows. The default does
    /// nothing, so a hook set written before these events existed compiles and behaves as it did.
    fn on_event(&self, _event: &HookEvent) -> HookAnswer {
        HookAnswer::Pass
    }

    /// Does any hook of this set ask for this event? The host asks before it builds one, so that a set with nothing to say about it costs a call that does nothing. The default is no.
    fn wants_event(&self, _event: &str) -> bool {
        false
    }
}

/// A call that only reads (every class it has is `READ`). A call with no classes is not taken for one.
pub fn is_read_only(call: &HookCall) -> bool {
    !call.classes.is_empty() && call.classes.iter().all(|c| c == "READ")
}

/// Something that watches what a call that is not a read does to files the host must be able to trust (card R6-10, T6-10.3): Aqua's tokens and permission files. The host asks it
/// just before such a call and tells it just after, and it can only tighten: make the calls after a change ask, never let one through.
pub trait CallWatch: Send + Sync {
    /// Just before a call: `Some(why)` when this call must ask the person, whatever the policy decided.
    fn before(&self, call: &HookCall) -> Option<String>;
    /// Just after a call that ran: `Some(note)` when this call is the one that was seen to change something (a short note for the model, shown as data).
    fn after(&self, call: &HookCall) -> Option<String>;
}

/// The person's hooks (if any) with a [`CallWatch`] beside them. The watch is asked first and always, so that it sees every call; a hook's refusal stands, a hook's question and
/// the watch's are put together, and the watch's note is added to the hook's.
pub struct Watched {
    inner: Option<std::sync::Arc<dyn Hooks>>,
    watch: std::sync::Arc<dyn CallWatch>,
}

impl Watched {
    pub fn new(inner: Option<std::sync::Arc<dyn Hooks>>, watch: std::sync::Arc<dyn CallWatch>) -> Self {
        Self { inner, watch }
    }
}

impl Hooks for Watched {
    fn pre_tool(&self, call: &HookCall) -> HookAnswer {
        let watch_says = self.watch.before(call);
        let hook_says = self.inner.as_ref().map(|h| h.pre_tool(call)).unwrap_or(HookAnswer::Pass);
        match (hook_says, watch_says) {
            (HookAnswer::Deny(why), _) => HookAnswer::Deny(why),
            (HookAnswer::Ask(hook), Some(watch)) => HookAnswer::Ask(format!("{watch}; also: {hook}")),
            (HookAnswer::Ask(hook), None) => HookAnswer::Ask(hook),
            (HookAnswer::Pass, Some(watch)) => HookAnswer::Ask(watch),
            (HookAnswer::Pass, None) => HookAnswer::Pass,
        }
    }

    fn post_tool(&self, call: &HookCall, ok: bool, text: &str) -> PostAnswer {
        let mut answer = self.inner.as_ref().map(|h| h.post_tool(call, ok, text)).unwrap_or_default();
        if let Some(note) = self.watch.after(call) {
            answer.note = Some(match answer.note.take() {
                Some(hook) => format!("{hook}; {note}"),
                None => note,
            });
        }
        answer
    }

    fn on_event(&self, event: &HookEvent) -> HookAnswer {
        self.inner.as_ref().map(|h| h.on_event(event)).unwrap_or(HookAnswer::Pass)
    }

    fn wants_event(&self, event: &str) -> bool {
        self.inner.as_ref().is_some_and(|h| h.wants_event(event))
    }
}
