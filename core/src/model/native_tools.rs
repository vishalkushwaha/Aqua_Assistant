//! Native tool calls as a selectable mode of the local model (card OA9-06, task PR-14 of the OpenCode audit).
//!
//! Aqua's planner names its tool in JSON that the server holds the reply to (`Output::JsonSchema`). Some model families are trained to call tools through the server's own tool-call
//! format instead (`tools` in the request, `tool_calls` in the reply). **This is a mode, not a replacement.** The default is the constrained JSON of today ([`ToolMode::ConstrainedJson`]),
//! and the mode is `Native` only where the person said so: `AQUA_PLANNER_TOOL_MODE=native` for every model, or `"tool_mode": "native"` (or `"json"`) in the entry of a model in `profiles.json` (read
//! with `AQUA_MODEL_PROFILES` on; the entry of a model wins over the switch). It applies to the agent's planner only; the chat, the decisions and the summaries are not changed.
//!
//! **What it does.** For one request of the planner, [`Offer`] is made from the tools the loop shows that round (the same list the JSON schema is made from) and no other source: a name, a
//! description cut at 1,000 characters and the schema of its arguments. The request to the server carries them as `tools` with `tool_choice: auto`, and the reply's `tool_calls` are
//! turned back into the text the planner's reader has always read (`{"tool":"fs.read","arguments":{...}}`, or `{"actions":[...]}` for more than one), so that **one call is one
//! call, whichever way it came**: the same reader, the same checks, the same `ToolHost::call` (validation, approval, masking, the audit line). Nothing in this file calls a tool.
//!
//! **What is refused, and how.** A reply that cannot be made into a call is not guessed at and not run: a tool that was not offered (also when it is written in the words of the reply
//! and not in `tool_calls`), arguments that are not a JSON object, a call with no name, more than eight calls, a call that is not a function. Each is a [`NativeError`], and the planner
//! hands the loop the text `{"error":"<the sentence>"}`, which the loop reads as a reply that is neither a call nor an answer: the recovery it has always had for a reply that cannot be
//! read (the model is told, the round is spent, the next round asks again). A reply with no call and no words is an empty answer, as it always was. An offer that cannot be made (two tools
//! whose names become one) means the request goes the JSON way.
//!
//! **What is not built.** `tool_choice: required` with an answer as a pseudo-tool (the audit's wording for the planner's turns): the model may answer in words instead of calling, and
//! that is read as the final answer, as before. A prompt without the text list of tools (the prompt is byte for byte the one of the JSON mode, so the tools are in it twice, which is
//! part of what the experiment measures). Streaming: only the planner's plain calls use it. **It has not been run on a model:** whether a build of `llama-server` takes `tools` together with
//! the server's template, and whether the model calls as well as it writes JSON, is what `scripts/eval/speed_experiments/run_experiment.py native_tools` is for.

use super::profile::Profiles;
use super::ToolMode;
use crate::flags;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// The most calls one reply may carry (the loop keeps `AQUA_PLANNER_BATCH` of them, one by default; more than this is a reply that is refused).
pub const MAX_CALLS: usize = 8;
/// The longest arguments of one call, in bytes.
pub const MAX_ARGUMENT_BYTES: usize = 1 << 20;
/// The longest description of a tool that goes to the server.
pub const MAX_DESCRIPTION_CHARS: usize = 1000;
/// The longest name of a tool.
pub const MAX_NAME_CHARS: usize = 64;

/// A tool as the loop shows it to the planner: the name, what it is for, the schema of its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// Why a reply, or a list of tools, cannot be used as native calls. Each is a sentence for the model and for the log, with nothing in it that the model did not write itself and that
/// is more than 64 characters (a name of a tool is cut).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeError {
    /// Two tools of the list have the same name once the name is made fit for the server.
    DuplicateName { first: String, second: String },
    /// A tool of the list has no name, or one over the limit.
    BadToolName,
    /// The reply names a tool that was not offered this round.
    UnknownTool { name: String },
    /// The arguments of a call are not a JSON object (or are too long).
    BadArguments { tool: String, why: &'static str },
    /// A call of the reply has no name or is not a function.
    MalformedCall { why: &'static str },
    /// More calls than a reply may carry.
    TooManyCalls { count: usize },
}

fn shown(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect()
}

impl NativeError {
    /// A word for the counts of the report.
    pub fn class(&self) -> &'static str {
        match self {
            NativeError::DuplicateName { .. } => "duplicate_name",
            NativeError::BadToolName => "bad_tool_name",
            NativeError::UnknownTool { .. } => "unknown_tool",
            NativeError::BadArguments { .. } => "bad_arguments",
            NativeError::MalformedCall { .. } => "malformed_call",
            NativeError::TooManyCalls { .. } => "too_many_calls",
        }
    }
}

impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeError::DuplicateName { first, second } => write!(f, "the tools {} and {} have the same name for the server", shown(first), shown(second)),
            NativeError::BadToolName => f.write_str("a tool has no name, or a name longer than 64 characters"),
            NativeError::UnknownTool { name } => write!(f, "the tool {:?} was not offered this round, so it was not run; call one of the tools you were shown", shown(name)),
            NativeError::BadArguments { tool, why } => write!(f, "the arguments of {} are not usable ({why}), so the call was not run", shown(tool)),
            NativeError::MalformedCall { why } => write!(f, "a tool call of the reply is not usable ({why}), so nothing was run"),
            NativeError::TooManyCalls { count } => write!(f, "the reply holds {count} tool calls, and at most {MAX_CALLS} are read, so nothing was run"),
        }
    }
}

impl std::error::Error for NativeError {}

/// The text the loop reads when a reply cannot be made into a call: one JSON object that is neither a call nor an answer.
pub fn unusable_text(error: &NativeError) -> String {
    json!({ "error": error.to_string() }).to_string()
}

struct Offered {
    real: String,
    wire: String,
    description: String,
    parameters: Value,
}

/// The tools of one request, fixed when it is made. There is no way to add to it or change it afterwards: what a tool result or a model says cannot reach it.
pub struct Offer {
    tools: Vec<Offered>,
    /// Both spellings of each name (the real one, `fs.read`, and the one for the server, `fs__read`) to the index of the tool.
    by_name: BTreeMap<String, usize>,
}

impl std::fmt::Debug for Offer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Offer").field("tools", &self.names()).finish()
    }
}

/// The name as the server's templates take it: letters, digits, `_` and `-`. A dot (`fs.read`) becomes two underscores, any other character `_xx_` with its code.
pub fn wire_name(real: &str) -> String {
    let mut out = String::new();
    for c in real.chars() {
        match c {
            c if c.is_ascii_alphanumeric() || c == '_' || c == '-' => out.push(c),
            '.' => out.push_str("__"),
            c => out.push_str(&format!("_{:x}_", c as u32)),
        }
    }
    out
}

impl Offer {
    /// The offer for these tools. A name that is empty or too long, or two names that become one, is an error and nothing is offered.
    pub fn new(specs: &[ToolSpec]) -> Result<Offer, NativeError> {
        let mut tools = Vec::new();
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        for spec in specs {
            if spec.name.trim().is_empty() || spec.name.chars().count() > MAX_NAME_CHARS || spec.name.chars().any(char::is_control) {
                return Err(NativeError::BadToolName);
            }
            let index = tools.len();
            let wire = wire_name(&spec.name);
            for key in [&spec.name, &wire] {
                if let Some(&other) = by_name.get(key.as_str()) {
                    if other != index {
                        let first: &Offered = &tools[other];
                        return Err(NativeError::DuplicateName { first: first.real.clone(), second: spec.name.clone() });
                    }
                }
            }
            by_name.insert(spec.name.clone(), index);
            by_name.insert(wire.clone(), index);
            let parameters = if spec.schema.is_object() && spec.schema.get("type").is_some() { spec.schema.clone() } else { json!({ "type": "object", "properties": {} }) };
            tools.push(Offered { real: spec.name.clone(), wire, description: spec.description.chars().take(MAX_DESCRIPTION_CHARS).collect(), parameters });
        }
        Ok(Offer { tools, by_name })
    }

    /// The real names of the tools, in order.
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.real.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The real name of the tool a reply names (by its real spelling or the server's), when it was offered.
    pub fn real_name(&self, named: &str) -> Option<&str> {
        self.by_name.get(named.trim()).map(|&i| self.tools[i].real.as_str())
    }

    /// The `tools` of the request, in the server's format.
    pub fn tools_json(&self) -> Value {
        Value::Array(self.tools.iter().map(|t| json!({ "type": "function", "function": { "name": t.wire, "description": t.description, "parameters": t.parameters } })).collect())
    }

    /// The body of a request with the offer in it: `tools` and `tool_choice: auto`, and no `response_format` or `grammar` (the server does not take a grammar of the caller with tools,
    /// and the reply is not held to the planner's schema in this mode). Everything else in the body is as it was.
    pub fn apply(&self, mut body: Value) -> Value {
        if let Some(object) = body.as_object_mut() {
            object.remove("response_format");
            object.remove("grammar");
            object.insert("tools".into(), self.tools_json());
            object.insert("tool_choice".into(), json!("auto"));
        }
        body
    }

    /// The calls of a reply message as the text the planner's reader reads, or `None` when the message carries no tool call. A call that cannot be used is an error, and nothing of the
    /// message is used then.
    pub fn text_of_calls(&self, message: &Value) -> Result<Option<String>, NativeError> {
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array).filter(|c| !c.is_empty()) else { return Ok(None) };
        if calls.len() > MAX_CALLS {
            return Err(NativeError::TooManyCalls { count: calls.len() });
        }
        let mut read = Vec::new();
        for call in calls {
            if call.get("type").and_then(Value::as_str).is_some_and(|t| t != "function") {
                return Err(NativeError::MalformedCall { why: "it is not a function call" });
            }
            let function = call.get("function").filter(|f| f.is_object()).ok_or(NativeError::MalformedCall { why: "it has no function" })?;
            let named = function.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).ok_or(NativeError::MalformedCall { why: "it has no name" })?;
            let real = self.real_name(named).ok_or_else(|| NativeError::UnknownTool { name: named.to_string() })?;
            let arguments = match function.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(Value::String(text)) if text.trim().is_empty() => json!({}),
                Some(Value::String(text)) if text.len() > MAX_ARGUMENT_BYTES => return Err(NativeError::BadArguments { tool: real.to_string(), why: "they are too long" }),
                Some(Value::String(text)) => serde_json::from_str::<Value>(text).map_err(|_| NativeError::BadArguments { tool: real.to_string(), why: "they are not JSON" })?,
                Some(other) => other.clone(),
            };
            if !arguments.is_object() {
                return Err(NativeError::BadArguments { tool: real.to_string(), why: "they are not a JSON object" });
            }
            read.push(json!({ "tool": real, "arguments": arguments }));
        }
        Ok(Some(if read.len() == 1 { read.remove(0).to_string() } else { json!({ "actions": read }).to_string() }))
    }

    /// For the words of a reply that has no tool call: a call written in them that names a tool that was not offered is refused too (in the JSON mode the schema made that impossible).
    /// Words that hold no call, or only calls of offered tools, pass.
    pub fn check_words(&self, words: &str) -> Result<(), NativeError> {
        let Some(object) = first_object(words) else { return Ok(()) };
        let mut names: Vec<&str> = Vec::new();
        if let Some(name) = object.get("tool").and_then(Value::as_str) {
            names.push(name);
        }
        if let Some(actions) = object.get("actions").and_then(Value::as_array) {
            names.extend(actions.iter().filter_map(|a| a.get("tool").and_then(Value::as_str)));
        }
        for name in names {
            if self.real_name(name).is_none() {
                return Err(NativeError::UnknownTool { name: name.to_string() });
            }
        }
        Ok(())
    }

    /// The reply of the server as the planner's reader wants it: the same JSON with the text of the message made from its calls (or its words), so that everything after it (the
    /// timings, the cut-off check, the cleaning of the text) is what it always is. A reply that cannot be used becomes [`unusable_text`]. Gives what was done, for the counts.
    pub fn rewrite_reply(&self, mut reply: Value) -> (Value, Outcome) {
        let message = reply.pointer("/choices/0/message").cloned().unwrap_or(Value::Null);
        let (text, outcome) = match self.text_of_calls(&message) {
            Ok(Some(text)) => (text, Outcome::Calls(message.get("tool_calls").and_then(Value::as_array).map_or(0, Vec::len))),
            Ok(None) => {
                let words = message.get("content").and_then(Value::as_str).unwrap_or("").to_string();
                match self.check_words(&words) {
                    Ok(()) => (words, Outcome::Words),
                    Err(e) => (unusable_text(&e), Outcome::Refused(e.class())),
                }
            }
            Err(e) => (unusable_text(&e), Outcome::Refused(e.class())),
        };
        if let Some(slot) = reply.pointer_mut("/choices/0/message") {
            if let Some(object) = slot.as_object_mut() {
                object.insert("content".into(), Value::String(text));
                object.remove("tool_calls");
            }
        }
        (reply, outcome)
    }
}

/// What a reply of the server turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// This many tool calls, turned into the planner's text.
    Calls(usize),
    /// Words only (an answer, or an empty reply), passed on as they were.
    Words,
    /// Refused, with the class of the error.
    Refused(&'static str),
}

/// The first JSON object in `text`, the way the loop's reader finds one: the whole text, a fenced block, or the first `{` from which a whole object can be read.
fn first_object(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let as_object = |v: Value| v.is_object().then_some(v);
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return as_object(v);
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after.strip_prefix("json").or_else(|| after.strip_prefix("JSON")).unwrap_or(after).trim_start();
        if let Some(end) = after.find("```") {
            if let Ok(v) = serde_json::from_str::<Value>(after[..end].trim()) {
                return as_object(v);
            }
        }
    }
    for (at, _) in trimmed.match_indices('{') {
        let mut stream = serde_json::Deserializer::from_str(&trimmed[at..]).into_iter::<Value>();
        if let Some(Ok(v)) = stream.next() {
            if v.is_object() {
                return Some(v);
            }
        }
    }
    None
}

// ---- which mode a model is spoken to in ---------------------------------------------------------------------------------------------------------------------

/// The mode for a model. The entry of the model in `profiles.json` wins when profiles are on and it names one; else the switch (`AQUA_PLANNER_TOOL_MODE=native`); else the constrained JSON.
pub fn mode_of(profile_mode: Option<ToolMode>, profiles_on: bool, switch_native: bool) -> ToolMode {
    match (profiles_on.then_some(profile_mode).flatten(), switch_native) {
        (Some(mode), _) => mode,
        (None, true) => ToolMode::Native,
        (None, false) => ToolMode::ConstrainedJson,
    }
}

/// Is the switch `AQUA_PLANNER_TOOL_MODE` at `native`?
pub fn switch_is_native() -> bool {
    flags::choice(&flags::TOOL_MODE) == "native"
}

/// The mode for the model in this file now: the switch, the profile of the model, and whether profiles are on.
pub fn mode_for_file(file_name: &str, profiles: &Profiles) -> ToolMode {
    let profile = profiles.for_file(file_name).and_then(|p| p.tool_mode());
    mode_of(profile, flags::is_on(&flags::MODEL_PROFILES), switch_is_native())
}

/// The mode for the model file at this path now: the profiles are read from the folder above the model folder, as the launch reads them (`profiles.json`, only with
/// `AQUA_MODEL_PROFILES` on). The launch asks it to know whether to start the server with the server's own templates.
pub fn mode_for_path(model: &Path) -> ToolMode {
    let Some(name) = model.file_name().and_then(|n| n.to_str()) else { return ToolMode::ConstrainedJson };
    let profiles = if flags::is_on(&flags::MODEL_PROFILES) {
        model.parent().and_then(Path::parent).map(|root| Profiles::load(&root.join(super::profile::FILE))).unwrap_or_default()
    } else {
        Profiles::default()
    };
    mode_for_file(name, &profiles)
}

/// Could any model be spoken to natively? A cheap look (two reads of the environment) that lets the loop skip making a list of tools for nothing.
pub fn possible() -> bool {
    switch_is_native() || flags::is_on(&flags::MODEL_PROFILES)
}

// ---- the tools of the round and the offer of the request, carried to the server code without a change of any signature ---------------------------------------------------

thread_local! {
    static CANDIDATES: RefCell<Option<Arc<Vec<ToolSpec>>>> = const { RefCell::new(None) };
    static OFFER: RefCell<Option<Arc<Offer>>> = const { RefCell::new(None) };
}

struct Restore<T: Clone + 'static>(&'static std::thread::LocalKey<RefCell<Option<T>>>, Option<T>);

impl<T: Clone + 'static> Drop for Restore<T> {
    fn drop(&mut self) {
        let back = self.1.take();
        self.0.with(|c| *c.borrow_mut() = back);
    }
}

/// Runs `f` with these tools as the tools of the round on this thread: the planner that is about to ask the model finds them with [`candidates`]. Empty is none.
pub fn with_candidates<R>(tools: Vec<ToolSpec>, f: impl FnOnce() -> R) -> R {
    let value = (!tools.is_empty()).then(|| Arc::new(tools));
    let _restore = Restore(&CANDIDATES, CANDIDATES.with(|c| c.replace(value)));
    f()
}

/// The tools of the round, when the loop gave some.
pub fn candidates() -> Option<Arc<Vec<ToolSpec>>> {
    CANDIDATES.with(|c| c.borrow().clone())
}

/// Runs `f` with this offer as the offer of the requests this thread makes (`None`: the JSON way).
pub fn with_offer<R>(offer: Option<Arc<Offer>>, f: impl FnOnce() -> R) -> R {
    let _restore = Restore(&OFFER, OFFER.with(|c| c.replace(offer)));
    f()
}

/// The offer of the request being made on this thread, if the planner made one.
pub fn current_offer() -> Option<Arc<Offer>> {
    OFFER.with(|c| c.borrow().clone())
}

// ---- what happened, for the report ------------------------------------------------------------------------------------------------------------------------------------

/// What happened to the requests that carried an offer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub requests: u64,
    pub calls: u64,
    pub words: u64,
    pub refused: BTreeMap<&'static str, u64>,
    pub fell_back: u64,
    pub offers_refused: u64,
}

impl Counts {
    /// A request went to the server with an offer, and its reply came back as this.
    pub fn add(&mut self, outcome: Outcome) {
        self.requests += 1;
        match outcome {
            Outcome::Calls(n) => self.calls += n as u64,
            Outcome::Words => self.words += 1,
            Outcome::Refused(class) => *self.refused.entry(class).or_insert(0) += 1,
        }
    }
}

/// How many requests in a row the server refused because they had tools, so that a server that cannot take them (a build or a template without them) is not asked first at every round.
pub struct Streak(AtomicU32);

/// After this many refusals in a row no more offers are made until the server is started again or a reply comes.
pub const REFUSALS_THAT_PAUSE: u32 = 2;

impl Streak {
    pub const fn new() -> Streak {
        Streak(AtomicU32::new(0))
    }

    pub fn refused(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn worked(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    pub fn allows(&self) -> bool {
        self.0.load(Ordering::Relaxed) < REFUSALS_THAT_PAUSE
    }
}

impl Default for Streak {
    fn default() -> Self {
        Streak::new()
    }
}

static COUNTS: Mutex<Counts> = Mutex::new(Counts { requests: 0, calls: 0, words: 0, refused: BTreeMap::new(), fell_back: 0, offers_refused: 0 });
static STREAK: Streak = Streak::new();

/// A request went to the server with an offer, and its reply came back as this. A reply that is words or calls shows the server takes tools.
pub fn note_reply(outcome: Outcome) {
    COUNTS.lock().unwrap_or_else(|e| e.into_inner()).add(outcome);
    if !matches!(outcome, Outcome::Refused(_)) {
        STREAK.worked();
    }
}

/// The server refused a request that had tools, and it was asked again the JSON way.
pub fn note_fallback() {
    COUNTS.lock().unwrap_or_else(|e| e.into_inner()).fell_back += 1;
    STREAK.refused();
}

/// A list of tools could not be made into an offer (two names became one): the request went the JSON way.
pub fn note_offer_refused() {
    COUNTS.lock().unwrap_or_else(|e| e.into_inner()).offers_refused += 1;
}

/// May an offer be made now? Not after two refusals in a row by the server, until it is started again.
pub fn offers_allowed() -> bool {
    STREAK.allows()
}

/// The server was started again (perhaps with `--jinja` now): offers may be made again.
pub fn forget_refusals() {
    STREAK.worked();
}

/// The report from what is given: the counts, whether offers are paused, the mode of the agent's model and what the switch says. Pure.
pub fn report_of(counts: &Counts, paused: bool, mode: ToolMode, switch_native: bool, agent_model: &str) -> Value {
    json!({
        "switch": if switch_native { "native" } else { "json" },
        "agent_model": agent_model,
        "agent_model_mode": if mode == ToolMode::Native { "native" } else { "json" },
        "applies_to": "the agent's planner only",
        "offers_paused_after_refusals": paused,
        "requests_with_tools": counts.requests,
        "tool_calls_read": counts.calls,
        "replies_in_words": counts.words,
        "replies_refused": counts.refused.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<String, Value>>(),
        "asked_again_the_json_way": counts.fell_back,
        "offers_not_made": counts.offers_refused,
    })
}

/// The mode and the counts, for the report of the speed experiments. `agent_model` is the file the agent's planner uses now.
pub fn report(agent_model: &str, profiles: &Profiles) -> Value {
    let counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    report_of(&counts, !offers_allowed(), mode_for_file(agent_model, profiles), switch_is_native(), agent_model)
}
