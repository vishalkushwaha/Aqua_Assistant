//! The seam between the agent loop and whatever writes its next step.
//!
//! The loop asks a [`Planner`] for the next action and reads the reply; it does not know which model, server or provider wrote it.
//! Today there is one, [`LlamaPlanner`] (the local llama.cpp server); the prompt it is given never leaves this machine. A planner on another model, or a remote one that the person has
//! chosen to allow (and that is asked before each call, not switched on by an environment variable), is another implementation of the same trait (roadmap card R3-02), not a change to the loop.
//!
//! What a planner may be asked to enforce is a [`PlanRequest::constraint`]: the reply must be JSON of the shape [`schema::action_schema`]
//! builds from the tools that were shown. A planner that cannot enforce one ignores it, and the loop reads the reply with the tolerant parser
//! it always had.

pub mod plan;
pub mod schema;

use crate::modules::llm_router::llama_server::Constraint;
use crate::modules::llm_router::LlamaRouterEngine;
use anyhow::Result;

/// One request for the next step.
pub struct PlanRequest<'a> {
    pub prompt: &'a str,
    /// The most tokens the reply may take.
    pub max_tokens: u32,
    /// What the reply must look like, when the planner can hold the model to it.
    pub constraint: Option<&'a Constraint>,
}

/// What came back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanReply {
    pub text: String,
    /// What the model server measured for this reply, when it says (the local server does; a planner that cannot leaves it empty).
    pub timings: Option<crate::modules::llm_router::llama_server::Timings>,
}

/// What went wrong with a request for the next step, in the classes the loop decides on (card OA4-02, task AG-10 of the OpenCode audit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanErrorKind {
    /// The model was working for another request and this one waited for it and was not served: asking again a little later may be.
    Busy,
    /// The model server could not be used (it would not start, it crashed): the next request starts it again.
    Unavailable,
    /// The prompt did not fit the window (the server refused it, or the window ended the reply): a shorter prompt may.
    ContextOverflow,
    /// The server refused the request for another reason: the same request is refused again.
    Rejected,
    /// The person's Stop (or a listener that left).
    Cancelled,
    /// Anything else: a time-out (asking again would spend the same time again), a reply that cannot be used, a fault.
    Other,
}

impl PlanErrorKind {
    /// The class of an error that came out of [`Planner::plan`]: the class it was given by the planner that made it, else the typed errors of the model layer, else the sentences the
    /// engine writes. An error that none of these names is [`PlanErrorKind::Other`], which is never asked again.
    pub fn of(error: &anyhow::Error) -> PlanErrorKind {
        use crate::model::ProviderError;
        if let Some(planned) = error.downcast_ref::<PlanError>() {
            return planned.kind;
        }
        if let Some(typed) = error.downcast_ref::<ProviderError>() {
            return match typed {
                ProviderError::ContextOverflow { .. } => PlanErrorKind::ContextOverflow,
                ProviderError::Rejected(_) | ProviderError::Auth(_) | ProviderError::Quota(_) | ProviderError::ContentPolicy(_) => PlanErrorKind::Rejected,
                ProviderError::Cancelled => PlanErrorKind::Cancelled,
                ProviderError::Overloaded { .. } | ProviderError::RateLimited { .. } => PlanErrorKind::Busy,
                ProviderError::Unavailable { .. } => PlanErrorKind::Unavailable,
                _ => PlanErrorKind::Other,
            };
        }
        if let Some(refused) = error.downcast_ref::<crate::modules::llm_router::llama_engine::RequestRefused>() {
            return match crate::model::error::classify_rejection(refused.0.clone()) {
                ProviderError::ContextOverflow { .. } => PlanErrorKind::ContextOverflow,
                _ => PlanErrorKind::Rejected,
            };
        }
        let text = format!("{error:#}");
        if text.contains("the task was cancelled") {
            PlanErrorKind::Cancelled
        } else if crate::modules::llm_router::llama_server::is_busy_text(&text) {
            PlanErrorKind::Busy
        } else if crate::model::error::context_overflow(&text).is_some() {
            PlanErrorKind::ContextOverflow
        } else {
            PlanErrorKind::Other
        }
    }

    /// May the same request be made again after a wait? (A prompt that did not fit is shortened and asked again by another rule.)
    pub fn worth_waiting_for(self) -> bool {
        matches!(self, PlanErrorKind::Busy | PlanErrorKind::Unavailable)
    }

    /// A word for logs and metric labels.
    pub fn name(self) -> &'static str {
        match self {
            PlanErrorKind::Busy => "busy",
            PlanErrorKind::Unavailable => "unavailable",
            PlanErrorKind::ContextOverflow => "context_overflow",
            PlanErrorKind::Rejected => "rejected",
            PlanErrorKind::Cancelled => "cancelled",
            PlanErrorKind::Other => "other",
        }
    }
}

/// An error of a request for the next step with its class. It reads and prints exactly as the error it wraps (`{}` and `{:#}` give the same text, and the chain below it is the same),
/// so what a person or a log sees does not change; [`PlanErrorKind::of`] finds the class by `downcast_ref`.
#[derive(Debug)]
pub struct PlanError {
    pub kind: PlanErrorKind,
    error: anyhow::Error,
}

impl PlanError {
    /// `error` with its class.
    pub fn wrap(kind: PlanErrorKind, error: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(PlanError { kind, error })
    }

    /// A new error with this class and message.
    pub fn new(kind: PlanErrorKind, message: impl Into<String>) -> anyhow::Error {
        PlanError::wrap(kind, anyhow::anyhow!(message.into()))
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for PlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(&*self.error)
    }
}

/// What a caller does about a prompt the model refused as too long (card OA9-02, task PR-03): it is given the refusal and returns the prompt of the same turn with less in it, or `None` when
/// nothing more can be taken out. A planner that asks again by itself calls it at most once for a request.
pub type PromptShrink<'a> = &'a mut dyn FnMut(&crate::model::ProviderError) -> Option<String>;

/// The same function for a shorter time, so that it can be handed on more than once (a reference to a trait object cannot be shortened inside an `Option` by the compiler on its own).
pub fn reborrowed<'s>(shrink: &'s mut Option<PromptShrink<'_>>) -> Option<PromptShrink<'s>> {
    match shrink {
        Some(f) => Some(&mut **f),
        None => None,
    }
}

pub trait Planner {
    /// The model's name, for the run record and the monitor.
    fn model_name(&self) -> String;
    /// The prompt must fit in this many tokens.
    fn max_context_tokens(&self) -> u32;
    /// Have the model read the start of its prompt now, so the request that follows finds it in its cache. A planner with no cache does nothing.
    fn warm_prefix(&self, prefix: &str) -> Result<()>;
    /// The next step. `on_tick` hears how many seconds the planner has been working, about once a second.
    fn plan(&self, req: &PlanRequest<'_>, on_tick: &mut dyn FnMut(u64)) -> Result<PlanReply>;
    /// [`Self::plan`], and a prompt the model refuses as too long may be asked again by the planner itself, once, with the shorter prompt `shrink` gives (the loop's own way of showing less of
    /// the earlier results). A planner that does not ask again by itself ignores `shrink`, and the loop recovers one level up, as it always did.
    fn plan_shrinking(&self, req: &PlanRequest<'_>, on_tick: &mut dyn FnMut(u64), _shrink: PromptShrink<'_>) -> Result<PlanReply> {
        self.plan(req, on_tick)
    }
}

/// The local model, through the llama.cpp server that Aqua starts and unloads when idle.
pub struct LlamaPlanner<'a>(pub &'a LlamaRouterEngine);

impl Planner for LlamaPlanner<'_> {
    fn model_name(&self) -> String {
        self.0.agent_model_filename()
    }

    fn max_context_tokens(&self) -> u32 {
        self.0.max_context_tokens()
    }

    fn warm_prefix(&self, prefix: &str) -> Result<()> {
        self.0.warm_agent_prefix(&self.model_name(), prefix)
    }

    fn plan(&self, req: &PlanRequest<'_>, on_tick: &mut dyn FnMut(u64)) -> Result<PlanReply> {
        self.plan_asking(req, on_tick, None)
    }

    fn plan_shrinking(&self, req: &PlanRequest<'_>, on_tick: &mut dyn FnMut(u64), shrink: PromptShrink<'_>) -> Result<PlanReply> {
        self.plan_asking(req, on_tick, Some(shrink))
    }
}

/// The offer of tools for this request, when the agent's model is spoken to in native tool calls and the loop gave the tools of the round: made from those tools and nothing else. `None` in
/// every other case, which is the usual way.
fn native_offer(engine: &LlamaRouterEngine) -> Option<std::sync::Arc<crate::model::native_tools::Offer>> {
    use crate::model::native_tools as native;
    let tools = native::candidates()?;
    if !native::offers_allowed() || native::mode_for_file(&engine.agent_model_filename(), engine.profiles()) != crate::model::ToolMode::Native {
        return None;
    }
    match native::Offer::new(&tools) {
        Ok(offer) if !offer.is_empty() => Some(std::sync::Arc::new(offer)),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("the tools of this round could not be offered as native tool calls ({e}); asking the JSON way");
            native::note_offer_refused();
            None
        }
    }
}

impl LlamaPlanner<'_> {
    fn plan_asking(&self, req: &PlanRequest<'_>, on_tick: &mut dyn FnMut(u64), shrink: Option<PromptShrink<'_>>) -> Result<PlanReply> {
        // The prompt goes to the local model and nowhere else: the AQUA_REMOTE_CODING* switches that once posted it to an https address are retired (card OA1-03). Someone who still
        // has one set is told once in the log.
        crate::modules::coding_runner::warn_once_if_retired_switches_set();
        // Whatever an earlier request left is not this one's.
        let _ = crate::modules::llm_router::llama_engine::take_last_timings();
        let _ = crate::modules::llm_router::llama_engine::take_server_down();
        let asked = match native_offer(self.0) {
            // The usual way, exactly as it was.
            None => self.0.generate_agent_response_shrinking(req.prompt, req.max_tokens, req.constraint, |secs| on_tick(secs), shrink),
            Some(offer) => {
                // Native tool calls (card OA9-06): the server is given the round's tools, and its reply's calls come back as the text the loop has always read. A server that refuses a request that
                // has tools (a build or a template that cannot take them) is asked again the JSON way, once, with the same prompt.
                let mut shrink = shrink;
                let first = crate::model::native_tools::with_offer(Some(offer), || self.0.generate_agent_response_shrinking(req.prompt, req.max_tokens, req.constraint, |secs| on_tick(secs), reborrowed(&mut shrink)));
                match first {
                    Err(e) if e.downcast_ref::<crate::modules::llm_router::llama_engine::RequestRefused>().is_some() => {
                        tracing::warn!("the model server refused a request that carried tools ({e}); asking again the JSON way");
                        crate::model::native_tools::note_fallback();
                        self.0.generate_agent_response_shrinking(req.prompt, req.max_tokens, req.constraint, |secs| on_tick(secs), shrink)
                    }
                    other => other,
                }
            }
        };
        match asked {
            Ok(text) => Ok(PlanReply { text, timings: crate::modules::llm_router::llama_engine::take_last_timings() }),
            Err(e) => {
                // The error of a call that went on to `llama-cli` after the server failed is the error of `llama-cli`, which does not say the server was the reason.
                let down = crate::modules::llm_router::llama_engine::take_server_down();
                let kind = match PlanErrorKind::of(&e) {
                    PlanErrorKind::Other if down => PlanErrorKind::Unavailable,
                    kind => kind,
                };
                Err(PlanError::wrap(kind, e))
            }
        }
    }
}

/// The constraint for a reply, or none when constraining is off.
pub fn constraint_for(tools: &[aqua_mcp_adapters::McpTool]) -> Option<Constraint> {
    constraint_for_goal(tools, "")
}

/// Like [`constraint_for`], and a coding goal may ask, recall, and attach a plan without the process-wide switches.
pub fn constraint_for_goal(tools: &[aqua_mcp_adapters::McpTool], goal: &str) -> Option<Constraint> {
    let mode = schema::mode();
    let extras = schema::Extras {
        thought: schema::thought_enabled(),
        ask: crate::modules::coding_session::allow_ask(goal),
        recall: crate::modules::coding_session::use_context_v2(goal),
        plan: crate::modules::coding_session::wants_plan(goal) || plan::enabled(),
    };
    mode.is_on().then(|| Constraint::JsonSchema(schema::action_schema_with(tools, mode, schema::max_batch(), extras)))
}

/// Is the last round of a run's budget held to an answer (`AQUA_LAST_STEP_FINAL=on`, card OA5-04, TL-10)? Off until it has been tried on the model.
pub fn last_step_final_enabled() -> bool {
    crate::flags::is_on(&crate::flags::LAST_STEP_FINAL)
}

/// The sentence added at the end of the prompt of the last round (card OA5-04, TL-10). It is at the end, with the other things that change from round to round, so that the
/// start of the prompt, which the model server keeps in its cache, is the same as in every other round.
pub const LAST_STEP_LINE: &str = "Last step: give the final answer now from what you have; say what is not done.";

/// The constraint of the last round of a run (card OA5-04, TL-10): the reply can only be an answer, or, where asking is on for this goal, the person's question. It names no tool,
/// so the last round cannot be spent on a call whose result nobody reads; it has no `recall` either, which would spend it on reading an earlier result again. `None` when constraining
/// is off (the sentence of [`LAST_STEP_LINE`] is then all there is).
pub fn constraint_for_last_step(goal: &str) -> Option<Constraint> {
    let mode = schema::mode();
    let extras = schema::Extras { thought: schema::thought_enabled(), ask: crate::modules::coding_session::allow_ask(goal), recall: false, plan: false };
    mode.is_on().then(|| Constraint::JsonSchema(schema::action_schema_with(&[], mode, 1, extras)))
}

/// What the model is told about the forms of reply beyond one call or an answer: it may chain calls (only while the reply is constrained
/// and a batch is allowed) and it may ask the person a question (only while that is switched on). Empty by default. It goes after the tool
/// list, so the start of the prompt (which the server keeps in its cache) is the same either way.
pub fn batch_hint() -> String {
    batch_hint_for("")
}

pub fn batch_hint_for(goal: &str) -> String {
    let n = schema::max_batch();
    let mut out = String::new();
    if schema::mode().is_on() && n >= 2 {
        out.push_str(&format!(
            "\nYou may also chain up to {n} steps in one reply: {{\"actions\":[{{\"tool\":\"NAME\",\"arguments\":{{...}}}},{{\"tool\":\"NAME\",\"arguments\":{{...}}}}]}}. \
             Do that only when a later step does not depend on what an earlier one returns (reading two files, for example).\n"
        ));
    }
    if crate::modules::coding_session::allow_ask(goal) {
        out.push_str("\nWhen the request is too unclear to act on (which of two files, which person) ask the user ONE short question instead of guessing: {\"ask\":\"your question\",\"choices\":[\"option\",\"option\"]}. Never ask about something a tool can find out.\n");
    }
    out
}

/// One test at a time may change the planner's environment settings (they are read from the process environment).
#[cfg(test)]
pub(crate) static TEST_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use super::TEST_ENV as ENV;

    #[test]
    fn by_default_the_reply_names_a_shown_tool_and_the_model_is_told_nothing_new() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        for v in ["AQUA_PLANNER_CONSTRAINT", "AQUA_PLANNER_BATCH", "AQUA_ASK"] {
            std::env::remove_var(v);
        }
        let tools = vec![aqua_mcp_adapters::McpTool { name: "fs.read".into(), description: String::new(), input_schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}), source: Some("host".into()) }];
        let Some(Constraint::JsonSchema(s)) = constraint_for(&tools) else { panic!("the default holds the reply to a schema") };
        assert_eq!(s["$defs"]["call"]["properties"]["tool"], serde_json::json!({"enum": ["fs.read"]}), "names only, the arguments are any object");
        assert_eq!(s["oneOf"].as_array().unwrap().len(), 2, "one call, or an answer: no batch, no question");
        assert_eq!(batch_hint(), "", "the prompt is exactly what it was");
        // It can be turned off, and then nothing is enforced.
        std::env::set_var("AQUA_PLANNER_CONSTRAINT", "off");
        assert!(constraint_for(&tools).is_none());
        std::env::set_var("AQUA_PLANNER_CONSTRAINT", "schema");
        assert!(matches!(constraint_for(&tools), Some(Constraint::JsonSchema(_))));
        std::env::set_var("AQUA_PLANNER_BATCH", "2");
        assert!(batch_hint().contains("chain up to 2 steps"), "{}", batch_hint());
        assert!(!batch_hint().contains("ask the user"), "no question form until it is switched on");
        std::env::set_var("AQUA_ASK", "on");
        assert!(batch_hint().contains("ask the user ONE short question"));
        let Some(Constraint::JsonSchema(s)) = constraint_for(&tools) else { panic!() };
        assert_eq!(s["oneOf"].as_array().unwrap().len(), 4, "call, batch, answer, question");
        for v in ["AQUA_PLANNER_CONSTRAINT", "AQUA_PLANNER_BATCH", "AQUA_ASK"] {
            std::env::remove_var(v);
        }
    }

    /// Card OA4-02 (AG-10): an error of a request for the next step is found to be one that a wait may cure, one that a shorter prompt may cure, or neither, from the types the engine
    /// gives it and from the sentences it writes; a class that was given by the planner wins; and nothing else about the error changes.
    #[test]
    fn the_class_of_an_error_is_found_from_its_type_or_its_sentence() {
        use crate::modules::llm_router::llama_engine::RequestRefused;
        use crate::modules::llm_router::llama_server::busy_message;
        let kind = |e: anyhow::Error| PlanErrorKind::of(&e);
        // The model server refused a prompt that is too long, or a request for another reason.
        assert_eq!(kind(anyhow::Error::new(RequestRefused("the language model refused this request (HTTP 400: exceeds the available context size (6144 tokens; the prompt has 6234))".into()))), PlanErrorKind::ContextOverflow);
        assert_eq!(kind(anyhow::Error::new(RequestRefused("the language model refused this request (HTTP 400: unsupported response_format)".into()))), PlanErrorKind::Rejected);
        // The engine's own typed overflow, and the model layer's other classes.
        use crate::model::ProviderError;
        assert_eq!(kind(anyhow::Error::new(ProviderError::ContextOverflow { limit: Some(6144), detail: "x".into() })), PlanErrorKind::ContextOverflow);
        assert_eq!(kind(anyhow::Error::new(ProviderError::Overloaded { retry_after: None })), PlanErrorKind::Busy);
        assert_eq!(kind(anyhow::Error::new(ProviderError::Cancelled)), PlanErrorKind::Cancelled);
        assert_eq!(kind(anyhow::Error::new(ProviderError::Unavailable { detail: "gone".into() })), PlanErrorKind::Unavailable);
        assert_eq!(kind(anyhow::Error::new(ProviderError::InvalidOutput("x".into()))), PlanErrorKind::Other);
        // By sentence.
        assert_eq!(kind(anyhow::anyhow!("stopped: the task was cancelled")), PlanErrorKind::Cancelled);
        assert_eq!(kind(anyhow::anyhow!(busy_message(120))), PlanErrorKind::Busy);
        assert_eq!(kind(anyhow::anyhow!("the language model refused this request (HTTP 400: exceeds the available context size (2048 tokens))")), PlanErrorKind::ContextOverflow);
        // Not worth asking again, or not known: a time-out would spend the same time again, a reply cut off by its own limit would be cut off again.
        for other in ["llama timed out after 90s (model=x, prompt was 5 chars)", "the model's reply was cut off at the token limit, so it cannot be used", "the model server did not answer", "", "the planner was asked more often than the test expected (2 requests, 1 replies were scripted)"] {
            assert_eq!(kind(anyhow::anyhow!(other.to_string())), PlanErrorKind::Other, "{other}");
        }
        assert!(PlanErrorKind::Busy.worth_waiting_for() && PlanErrorKind::Unavailable.worth_waiting_for());
        for never in [PlanErrorKind::ContextOverflow, PlanErrorKind::Rejected, PlanErrorKind::Cancelled, PlanErrorKind::Other] {
            assert!(!never.worth_waiting_for(), "{}", never.name());
        }
    }

    #[test]
    fn a_class_given_by_the_planner_wins_and_the_error_reads_as_it_did() {
        let plain = anyhow::anyhow!("the root cause").context("what the loop was doing");
        let (before, before_chain) = (plain.to_string(), format!("{plain:#}"));
        assert_eq!((before.as_str(), before_chain.as_str()), ("what the loop was doing", "what the loop was doing: the root cause"));
        let classed = PlanError::wrap(PlanErrorKind::Unavailable, plain);
        assert_eq!(PlanErrorKind::of(&classed), PlanErrorKind::Unavailable);
        assert_eq!((classed.to_string(), format!("{classed:#}")), (before, before_chain), "the same text, and the same chain");
        // The given class wins over what the sentence says.
        let classed = PlanError::new(PlanErrorKind::Busy, "stopped: the task was cancelled");
        assert_eq!(PlanErrorKind::of(&classed), PlanErrorKind::Busy);
        assert_eq!(classed.to_string(), "stopped: the task was cancelled");
        // The wrapped error can still be found by its type.
        let wrapped = PlanError::wrap(PlanErrorKind::Rejected, anyhow::Error::new(std::fmt::Error));
        assert!(wrapped.downcast_ref::<PlanError>().is_some());
    }

    #[test]
    fn a_constrained_request_carries_the_schema_and_an_unconstrained_one_is_unchanged() {
        use crate::modules::llm_router::llama_server::{request_body_constrained, request_body_for_slot};
        let plain = request_body_for_slot("hi", 8, Some(1));
        assert_eq!(request_body_constrained("hi", 8, Some(1), None), plain, "no constraint, the same request byte for byte");
        assert!(plain.get("response_format").is_none() && plain.get("grammar").is_none());
        let schema = serde_json::json!({"type": "object"});
        let with = request_body_constrained("hi", 8, Some(1), Some(&Constraint::JsonSchema(schema.clone())));
        assert_eq!(with["response_format"]["type"], "json_schema");
        assert_eq!(with["response_format"]["json_schema"]["schema"], schema);
        assert_eq!(with["id_slot"], 1, "the rest of the request is as it was");
        assert_eq!(with["cache_prompt"], true);
        let g = request_body_constrained("hi", 8, None, Some(&Constraint::Grammar("root ::= \"a\"".into())));
        assert_eq!(g["grammar"], "root ::= \"a\"");
        assert!(g.get("response_format").is_none());
    }
}
