//! The JSON of one request to the model server: the cache slots, the restriction on what the model may write, the tools of an offer, what only the model layer adds, and the question for the probabilities (move only: this was part of `llama_server.rs`).

use super::*;

/// The JSON body for one planner request. `cache_prompt` is what makes the second step cheap.
pub fn request_body(prompt: &str, max_tokens: u32) -> Value {
    request_body_for_slot(prompt, max_tokens, None)
}

/// The cache slot for chat requests and the one for the agent's planner. The server keeps what it has read in one slot per
/// parallel request; left to choose, it picks the slot that shares the most with the new prompt, and when nothing is shared it
/// takes the one used longest ago, which could be the agent's (a chat turn then threw away 1,500 tokens of agent prompt).
pub const CHAT_SLOT: u32 = 0;
pub const AGENT_SLOT: u32 = 1;

/// `wanted` when the server has a slot for each kind of request, and no preference when it has only one.
pub fn slot_for(wanted: u32) -> Option<u32> {
    (slots() >= 2).then_some(wanted)
}

/// A restriction on what the model may write, applied while it decodes (the server only lets it pick tokens that keep the reply valid).
#[derive(Debug, Clone, PartialEq)]
pub enum Constraint {
    /// The reply must be JSON that fits this schema. Sent as `response_format`, which the server turns into a grammar.
    JsonSchema(Value),
    /// The reply must match this GBNF grammar.
    Grammar(String),
}

/// Like [`request_body`], for a given cache slot (`id_slot` in the server's request).
pub fn request_body_for_slot(prompt: &str, max_tokens: u32, slot: Option<u32>) -> Value {
    request_body_constrained(prompt, max_tokens, slot, None)
}

/// Like [`request_body_for_slot`], and the reply is held to `constraint` while it is written (none: as before, byte for byte).
pub fn request_body_constrained(prompt: &str, max_tokens: u32, slot: Option<u32>, constraint: Option<&Constraint>) -> Value {
    request_body_with(prompt, max_tokens, slot, constraint, THINKING_ALLOWED.load(std::sync::atomic::Ordering::SeqCst))
}

/// Does the running server let the model think? Only when it was started with `AQUA_LLAMA_THINK_BUDGET` above 0 (experiment E6 of card R3-01); false otherwise, and the requests then say that
/// thinking is off, as they always did. Set when a server has started (see `launch`).
pub(super) static THINKING_ALLOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [`request_body_constrained`] with the choice of thinking made by the caller: `thinking` false is the body that was always sent.
pub(crate) fn request_body_with(prompt: &str, max_tokens: u32, slot: Option<u32>, constraint: Option<&Constraint>, thinking: bool) -> Value {
    let mut body = json!({
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": max_tokens,
        // Tool calls are JSON: a low temperature keeps the plan steady from one run to the next.
        "temperature": 0.3,
        "top_p": 0.9,
        "stream": false,
        "cache_prompt": true,
        "chat_template_kwargs": { "enable_thinking": thinking }
    });
    if let Some(slot) = slot {
        body["id_slot"] = json!(slot);
    }
    match constraint {
        Some(Constraint::JsonSchema(schema)) => body["response_format"] = json!({ "type": "json_schema", "json_schema": { "name": "aqua_action", "strict": true, "schema": schema } }),
        Some(Constraint::Grammar(g)) => body["grammar"] = json!(g),
        None => {}
    }
    body
}

/// The body of a request with the tools of an offer in it (card OA9-06), or [`build_body`] exactly as it always was when there is none. With an offer the reply is not held to the
/// schema: the server is given the tools instead (`native_tools::Offer::apply`).
pub(crate) fn body_with_offer(prompt: &str, max_tokens: u32, slot: Option<u32>, constraint: Option<&Constraint>, logprobs: Option<u32>, extras: &Extras<'_>, offer: Option<&crate::model::native_tools::Offer>) -> Value {
    match offer {
        Some(offer) => offer.apply(build_body(prompt, max_tokens, slot, None, logprobs, extras)),
        None => build_body(prompt, max_tokens, slot, constraint, logprobs, extras),
    }
}

/// What only the model layer (`crate::model`) adds to a request. `Extras::default()` is the request the engine has always made: the same body byte for byte, ended only by the
/// turn being stopped.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Extras<'a> {
    /// `Some`: whether the model may think for this one request. `None`: what the running server was started with, as [`request_body_constrained`] has it.
    pub thinking: Option<bool>,
    /// Fields written into the body after it is built and before the probabilities are asked for (sampling: `temperature`, `top_k`, `seed`...). A key the body already has is replaced,
    /// and a null takes the key out.
    pub fields: Option<&'a serde_json::Map<String, Value>>,
    /// The caller's own flag: set, it ends the request exactly as the person's Stop does ([`Failure::Cancelled`]).
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// How long a streamed answer may be quiet once its first event has come (the watchdog, see [`stream_watch`]). `None`: `AQUA_STREAM_IDLE_SECS`, which is off unless it is set.
    pub idle: Option<Duration>,
    /// How long a streamed call may wait for its first event. `None`: `AQUA_STREAM_FIRST_EVENT_SECS`, off unless it is set.
    pub first_event: Option<Duration>,
}

/// Has the turn been stopped (the person's Stop), or has the caller of the model layer set its own flag?
pub(super) fn stopped(extras: &Extras<'_>) -> bool {
    crate::modules::browser_agent::turn_cancelled() || extras.cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst))
}

/// The whole JSON body of one request: [`request_body_with`], then the extra fields, then the question for the probabilities. The one place this is put together, so that the request the
/// model layer describes and the one that is sent cannot differ.
pub(crate) fn build_body(prompt: &str, max_tokens: u32, slot: Option<u32>, constraint: Option<&Constraint>, logprobs: Option<u32>, extras: &Extras<'_>) -> Value {
    let thinking = extras.thinking.unwrap_or_else(|| THINKING_ALLOWED.load(std::sync::atomic::Ordering::SeqCst));
    let mut body = request_body_with(prompt, max_tokens, slot, constraint, thinking);
    if let Some(fields) = extras.fields {
        for (key, value) in fields {
            // A null takes the field out of the body: a model whose template has no thinking switch is not sent one (card OA9-03).
            match (value.is_null(), body.as_object_mut()) {
                (true, Some(object)) => {
                    object.remove(key.as_str());
                }
                _ => body[key.as_str()] = value.clone(),
            }
        }
    }
    with_logprobs(body, logprobs)
}

/// The request with the probabilities of the `top` likeliest tokens asked for, and a decode that does not choose by chance (a decision is read from the numbers, and the
/// same question must give the same answer).
pub fn with_logprobs(mut body: Value, top: Option<u32>) -> Value {
    if let Some(top) = top {
        body["logprobs"] = json!(true);
        body["top_logprobs"] = json!(top.clamp(1, 20));
        body["temperature"] = json!(0);
        body["top_p"] = json!(1);
    }
    body
}

/// A grammar that lets the model write one of `labels` and nothing else (`root ::= "A" | "B"`).
pub fn choice_grammar(labels: &[String]) -> String {
    let quoted: Vec<String> = labels.iter().map(|l| format!("\"{}\"", l.replace('\\', "\\\\").replace('"', "\\\""))).collect();
    format!("root ::= {}", quoted.join(" | "))
}
