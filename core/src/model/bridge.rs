//! Where the engine asks the model server. The engine (`LlamaRouterEngine`) still decides which file answers, writes the prompt, cleans the reply and falls back to `llama-cli`;
//! the five calls it makes to the server go through here.
//!
//! With `AQUA_MODEL_ROUTER` off (the default) each function calls the server function of the same name with the same arguments: nothing changes. With it on the call is described as a
//! [`ChatRequest`] and goes through the router to the provider for the local server, which sends the same body, and the answer is turned back into the `Reply` or `Failure` the engine
//! reads. The conversions lose nothing (a test runs a reply through them), so the engine's behaviour does not depend on the switch.
//!
//! **The fast tier** (card OA9-06, second round): with `AQUA_FAST_TIER` on, a call of the role decision or fast also takes the router way, through `fast_route`, which asks the second
//! model server first and the main model when it cannot answer. With the switch off `fast_route::tier_for` says none after reading that one switch, and every call is what it was.

use super::error::ProviderError;
use super::sampling::{self, ModelSettings};
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, FinishReason, Flow, Message, ModelRef, Output, Purpose, Role, Usage};
use super::{current_router, fast_route, llama_server_provider, router_enabled};
use crate::modules::llm_router::llama_server::{self as server, Constraint, Extras, Failure, Reply, Timings};
use crate::modules::planner::PromptShrink;
use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;

thread_local! {
    /// The settings of the call this thread is making (card OA9-03), put there by [`with_settings`] around the engine's call and read by the five functions below.
    static SETTINGS: RefCell<Option<ModelSettings>> = const { RefCell::new(None) };
}

/// Runs `f` with `settings` as the settings of the calls this thread makes through the bridge, and puts back what was there when it ends, however it ends. `None` is the request the
/// engine has always sent. The function signatures below do not change: the engine decides the settings (it knows the model, the role and the person's files) and the bridge only
/// carries them.
pub(crate) fn with_settings<R>(settings: Option<ModelSettings>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<ModelSettings>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SETTINGS.with(|s| *s.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(SETTINGS.with(|s| s.replace(settings)));
    f()
}

fn current_settings() -> Option<ModelSettings> {
    SETTINGS.with(|s| s.borrow().clone())
}

/// What the engine asks the server for, in the engine's own terms.
struct Ask<'a> {
    role: Role,
    purpose: Purpose,
    model: &'a Path,
    prompt: &'a str,
    max_tokens: u32,
    slot: Option<u32>,
    output: Output,
    stream: bool,
    timeout: Duration,
    /// What to do about a prompt the server refuses as too long: the shorter prompt of the same turn (card OA9-02). The router asks again with it, once.
    shrink: Option<PromptShrink<'a>>,
}

/// `Output` for a constraint the engine already has.
pub(crate) fn output_of(constraint: Option<&Constraint>) -> Output {
    match constraint {
        None => Output::Text,
        Some(Constraint::JsonSchema(schema)) => Output::JsonSchema(schema.clone()),
        Some(Constraint::Grammar(grammar)) => Output::Grammar(grammar.clone()),
    }
}

/// What the server measured, from the usage of a call (the two hold the same five numbers).
pub(crate) fn timings_of(u: &Usage) -> Timings {
    Timings { prompt_n: u.input, cache_n: u.cached, predicted_n: u.output, prompt_ms: u.prompt_ms, predicted_ms: u.decode_ms }
}

/// The server's reply from the outcome of a call (the numbers the server measured are kept as they were).
pub(crate) fn reply_of(outcome: ChatOutcome) -> Reply {
    let u = outcome.usage;
    Reply {
        timings: timings_of(&u),
        text: outcome.text,
        cut_off: outcome.finish == FinishReason::Length,
        prompt_tokens_read: u.input,
        prompt_tokens_cached: u.cached,
        first_token_probs: outcome.choice_probs,
    }
}

/// One call through the router. `on_text` is handed each piece of a streamed answer and returns `false` to hang up. `tier` is the fast tier of this call (card OA9-06), when it has one:
/// the second server is asked first and the main model when it cannot answer.
fn ask(call: Ask<'_>, settings: Option<&ModelSettings>, tier: Option<fast_route::Tier>, on_tick: &mut dyn FnMut(u64), mut on_text: Option<&mut dyn FnMut(&str) -> bool>) -> Result<Reply, Failure> {
    let messages = [Message::user(call.prompt)];
    let mut req = ChatRequest::new(&messages, call.max_tokens, call.purpose).with_slot(call.slot).with_output(call.output);
    req.stream = call.stream;
    if let Some(s) = settings {
        req.sampling = s.sampling.clone();
        req.reasoning = s.reasoning;
    }
    // The caller's shorter prompt is one message, as the prompt it replaces was.
    let mut shorter_turn = call.shrink.map(|shrink| move |refusal: &ProviderError| shrink(refusal).map(|prompt| vec![Message::user(prompt)]));
    let mut ctl = CallControl::with_deadline(call.timeout).with_ticks(on_tick);
    if let Some(shorter) = shorter_turn.as_mut() {
        ctl = ctl.with_shrink(shorter);
    }
    let mut sink = |event: ChatEvent| -> Flow {
        if let (ChatEvent::TextDelta(piece), Some(deliver)) = (&event, on_text.as_mut()) {
            if !(**deliver)(piece) {
                return Flow::Stop;
            }
        }
        Flow::Continue
    };
    let model = ModelRef::new(llama_server_provider::ID, call.model.to_string_lossy());
    let result = match tier {
        Some(tier) => tier.call(&fast_route::router(), call.role, &req, &mut ctl, &mut sink),
        None => current_router().call_on(&model, call.role, &req, &mut ctl, &mut sink),
    };
    result.map(reply_of).map_err(|e: ProviderError| Failure::from(e))
}

/// One call straight to the server (the router is off) that carries settings: the request `server::complete_*` would send, with the body fields the settings name.
fn direct(call: Ask<'_>, settings: &ModelSettings, on_tick: &mut dyn FnMut(u64), on_text: Option<&mut dyn FnMut(&str) -> bool>) -> Result<Reply, Failure> {
    let (constraint, logprobs) = llama_server_provider::constraint_of(&call.output);
    let (thinking, fields) = sampling::body_parts(&settings.sampling, settings.reasoning);
    server::complete_inner(call.model, call.prompt, call.max_tokens, call.slot, constraint.as_ref(), logprobs, call.timeout, on_tick, on_text, &Extras { thinking, fields: Some(&fields), ..Extras::default() })
}

/// The call as the router or the server is asked for it: through the router when it is on or the call has a fast tier, else straight to the server with the settings.
fn route(call: Ask<'_>, settings: Option<ModelSettings>, on_tick: &mut dyn FnMut(u64), on_text: Option<&mut dyn FnMut(&str) -> bool>) -> Result<Reply, Failure> {
    let tier = fast_route::tier_for(call.role, call.model);
    if router_enabled() || tier.is_some() {
        return ask(call, settings.as_ref(), tier, on_tick, on_text);
    }
    // No settings is the request the server function of the call's name would send (a test holds the two to the same bytes).
    direct(call, &settings.unwrap_or_default(), on_tick, on_text)
}

/// [`server::complete_choice`]: one decode step held to a closed set of words, with the probabilities of the first token (a decision).
pub(crate) fn complete_choice(model: &Path, prompt: &str, labels: &[String], slot: Option<u32>, timeout: Duration) -> Result<Reply, Failure> {
    let settings = current_settings();
    if !router_enabled() && settings.is_none() && !fast_route::applies(Role::Decision) {
        return server::complete_choice(model, prompt, labels, slot, timeout);
    }
    let call = Ask { role: Role::Decision, purpose: Purpose::Decision, model, prompt, max_tokens: 1, slot, output: Output::Choice(labels.to_vec()), stream: false, timeout, shrink: None };
    route(call, settings, &mut |_| {}, None)
}

/// [`server::complete_streaming`]: the answer is also handed to `on_text` as it is written; `on_text` returns `false` to stop.
pub(crate) fn complete_streaming(model: &Path, prompt: &str, max_tokens: u32, slot: Option<u32>, timeout: Duration, mut on_tick: impl FnMut(u64), mut on_text: impl FnMut(&str) -> bool) -> Result<Reply, Failure> {
    let settings = current_settings();
    if !router_enabled() && settings.is_none() {
        return server::complete_streaming(model, prompt, max_tokens, slot, timeout, on_tick, on_text);
    }
    let call = Ask { role: Role::Chat, purpose: Purpose::Chat, model, prompt, max_tokens, slot, output: Output::Text, stream: true, timeout, shrink: None };
    route(call, settings, &mut on_tick, Some(&mut on_text))
}

/// [`server::complete_constrained`], for the planner (`role` Agent) or for chat (`role` Chat): the reply is held to `constraint` as it is written.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn complete_constrained(role: Role, model: &Path, prompt: &str, max_tokens: u32, slot: Option<u32>, constraint: Option<&Constraint>, timeout: Duration, on_tick: impl FnMut(u64)) -> Result<Reply, Failure> {
    complete_constrained_shrinking(role, model, prompt, max_tokens, slot, constraint, timeout, on_tick, None)
}

/// [`complete_constrained`], and with the router on a prompt the server refuses as too long is asked again once with the shorter prompt `shrink` gives, which is a whole prompt as
/// `prompt` is (card OA9-02, task PR-03). With the router off the server is called as it always was and `shrink` is not.
#[allow(clippy::too_many_arguments)]
pub(crate) fn complete_constrained_shrinking(role: Role, model: &Path, prompt: &str, max_tokens: u32, slot: Option<u32>, constraint: Option<&Constraint>, timeout: Duration, mut on_tick: impl FnMut(u64), mut shrink: Option<PromptShrink<'_>>) -> Result<Reply, Failure> {
    let settings = current_settings();
    if !router_enabled() && settings.is_none() && !fast_route::applies(role) {
        return server::complete_constrained(model, prompt, max_tokens, slot, constraint, timeout, on_tick);
    }
    let purpose = if role == Role::Agent { Purpose::Planner } else { Purpose::Chat };
    let call = Ask { role, purpose, model, prompt, max_tokens, slot, output: output_of(constraint), stream: false, timeout, shrink: crate::modules::planner::reborrowed(&mut shrink) };
    route(call, settings, &mut on_tick, None)
}

/// [`server::complete_checked`] as the warm-up uses it: read `prompt` into the cache, write one token, throw it away.
pub(crate) fn complete_warmup(role: Role, model: &Path, prompt: &str, slot: Option<u32>, timeout: Duration) -> Result<Reply, Failure> {
    let settings = current_settings();
    if !router_enabled() && settings.is_none() {
        return server::complete_checked(model, prompt, 1, slot, timeout, |_| {});
    }
    let call = Ask { role, purpose: Purpose::Warmup, model, prompt, max_tokens: 1, slot, output: Output::Text, stream: false, timeout, shrink: None };
    route(call, settings, &mut |_| {}, None)
}
