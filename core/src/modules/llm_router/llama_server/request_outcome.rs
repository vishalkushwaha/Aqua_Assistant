//! What a request ends in: the answer the model server gave (the text, what it measured, what it took from its cache) or the way it failed, and the sentences a request that was refused, was busy, gave the model up or went quiet says (move only: this was part of `llama_server.rs`).

use super::*;

/// Why a request did not produce an answer.
#[derive(Debug)]
pub enum Failure {
    /// The model was still working when the time limit ran out, or a streamed answer went quiet ([`is_idle_text`]). Trying another way would only
    /// spend the same time again, so the caller reports it.
    Timeout(String),
    /// The server could not be used (missing, would not start, crashed). The caller falls back.
    Unavailable(String),
    /// The server is fine and refused this one request (for example a prompt longer than its context). Nothing is wrong
    /// with the model, so it stays loaded, and no other way of running it would accept the same prompt either.
    Rejected(String),
    /// The turn was told to stop. The request was hung up; the server is healthy and stays loaded for the next task.
    Cancelled,
}

/// The most characters a prompt may have, whatever the window: it is a guard against a runaway paste, not a fit to the window, and `llama-cli` takes the prompt on its command line,
/// which Windows cuts off near 32 K characters. The limit that is in force is [`prompt_chars_ceiling`], which is this or less.
pub const MAX_PROMPT_CHARS: usize = 30_000;

/// A prompt longer than this is refused before it is sent: six characters a token is more than any text the model reads costs (the token meter keeps the cost of a character
/// between 1.5 and 4.5), so no prompt this long fits the window
/// ([`context_tokens`], 6144 tokens unless `AQUA_LLAMA_CTX` says otherwise, divided between the slots), and the limit is [`MAX_PROMPT_CHARS`] at the usual window and less at a
/// small one. A prompt between what really fits (about three characters a token, some 18,000 characters at 6144, more for prose) and this is sent and the server itself refuses it
/// (HTTP 400, [`Failure::Rejected`]). The planner builds its own prompts to the window it is told (`browser_agent::prompt_fitted_to_context_reserving`).
pub fn prompt_chars_ceiling() -> usize {
    ceiling_for(context_tokens())
}

pub(super) fn ceiling_for(window: u32) -> usize {
    (window as usize).saturating_mul(6).min(MAX_PROMPT_CHARS)
}

pub fn too_long(chars: usize) -> String {
    format!("that is too much text for the language model at once ({chars} characters; the limit is {}). Please shorten it.", prompt_chars_ceiling())
}

/// The tokens the prompt had and the window it did not fit, from the body of the server's refusal of a prompt that is too long (`"type":"exceed_context_size_error"`, with
/// `n_prompt_tokens` and `n_ctx`). The text is read for the two numbers and not parsed as JSON, because the error text keeps only the first 300 characters of the body.
pub fn overflow_counts(error: &str) -> Option<(u64, u64)> {
    if !error.contains("exceed_context_size_error") && !error.contains("exceeds the available context size") {
        return None;
    }
    let number_after = |key: &str| -> Option<u64> {
        let at = error.find(key)? + key.len();
        let digits: String = error[at..].chars().skip_while(|c| c.is_whitespace()).take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    Some((number_after("\"n_prompt_tokens\":")?, number_after("\"n_ctx\":")?))
}

/// The sentence for a request the server refused (`error` is its `HTTP 4xx: body`). A prompt that did not fit says how many tokens it had and how many the window holds, in words
/// that the model layer reads as a context overflow (`model::error::context_overflow`) and the meter reads for the count (`context::meter::refusal_prompt_tokens`); anything else
/// keeps the first 160 characters of what the server said.
pub fn refusal_message(error: &str) -> String {
    match overflow_counts(error) {
        Some((prompt, window)) => format!("the language model refused this request (HTTP 400: exceeds the available context size ({window} tokens; the prompt has {prompt}))"),
        None => format!("the language model refused this request ({})", error.chars().take(160).collect::<String>()),
    }
}

/// Is this error text an HTTP 4xx from the server (it answered, and said no to this request)?
pub(crate) fn is_client_rejection(error: &str) -> bool {
    error.strip_prefix("HTTP 4").is_some_and(|rest| rest.len() >= 2 && rest.as_bytes()[..2].iter().all(u8::is_ascii_digit))
}

/// The message of a request that waited for the model, which was working for another request, and was not served.
pub fn busy_message(waited_secs: u64) -> String {
    format!("the language model is busy with another request (waited {waited_secs}s)")
}

/// Is this text the message of [`busy_message`]? (Asking again a little later may be served; the planner does.)
pub fn is_busy_text(text: &str) -> bool {
    text.contains("the language model is busy with another request (waited ")
}

/// What a request that gave the model up says in its error. The agent loop reads it with [`is_yield`] and asks again at the next step boundary; it is not the person's Stop (that
/// message is "the task was cancelled"), so the run does not end. It travels as a time-out because every caller already hands a time-out's text on unchanged.
const YIELD_MARK: &str = "gave the model up to a person who was waiting";

/// What the error of a call that stopped so that a person could have the model says.
pub fn yielded_message() -> String {
    format!("{YIELD_MARK}: this call stopped and is made again when the person has been served")
}

/// The failure of a call that stopped so that a person could have the model.
pub fn yielded_failure() -> Failure {
    Failure::Timeout(yielded_message())
}

/// Is this error the one of [`yielded_failure`]?
pub fn is_yield(error: &anyhow::Error) -> bool {
    is_yield_text(&format!("{error:#}"))
}

/// Is this text the message of [`yielded_failure`]?
pub fn is_yield_text(text: &str) -> bool {
    text.contains(YIELD_MARK)
}

/// What a stream that went quiet says in its error. The model layer reads it with [`is_idle_text`] (`ProviderError::Timeout { idle: true }`). Like the yield, it travels as a time-out,
/// because every caller already hands a time-out's text on unchanged.
const IDLE_MARK: &str = "the language model went quiet";

/// The error of a streamed call that was hung up on because nothing came for too long (card OA9-02, task PR-03): `before_first` when not even the first event came (the server never
/// began to answer), else in the middle of the answer. `kept`: the server answered its health check afterwards and keeps its model.
pub fn idle_message(before_first: bool, quiet_secs: u64, kept: bool) -> String {
    format!(
        "{IDLE_MARK} {}: nothing came for {quiet_secs}s ({}). The request was hung up on and the model server {}.",
        if before_first { "before it began to answer" } else { "in the middle of the answer" },
        if before_first { "AQUA_STREAM_FIRST_EVENT_SECS" } else { "AQUA_STREAM_IDLE_SECS" },
        if kept { "is still loaded" } else { "did not answer its health check and was stopped" }
    )
}

/// Is this text the message of [`idle_message`]?
pub fn is_idle_text(text: &str) -> bool {
    text.contains(IDLE_MARK)
}

/// The answer text and how much of the prompt came from the cache, from a chat-completions reply.
pub fn parse_reply(v: &Value) -> Option<(String, u64, u64)> {
    let text = v.pointer("/choices/0/message/content")?.as_str()?.to_string();
    let read = v.pointer("/timings/prompt_n").and_then(|x| x.as_u64()).unwrap_or(0);
    let cached = v.pointer("/timings/cache_n").and_then(|x| x.as_u64()).unwrap_or(0);
    Some((text, read, cached))
}

/// True when the model stopped because it ran out of tokens (`finish_reason: "length"`) instead of finishing. A tool call or a
/// JSON answer cut off like that is unusable, and spoken text ends in the middle of a sentence.
pub fn reply_was_cut_off(v: &Value) -> bool {
    v.pointer("/choices/0/finish_reason").and_then(|x| x.as_str()) == Some("length")
}

/// What the server measured for one request (its `timings`), all whole numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timings {
    /// Prompt tokens the server had to read, and tokens it took from its cache.
    pub prompt_n: u64,
    pub cache_n: u64,
    /// Tokens it wrote.
    pub predicted_n: u64,
    /// Milliseconds spent reading the prompt (the wait before the first token) and writing the answer.
    pub prompt_ms: u64,
    pub predicted_ms: u64,
}

impl Timings {
    /// Tokens written per second, 0 when nothing was measured.
    pub fn decode_tokens_per_second(&self) -> f64 {
        if self.predicted_ms == 0 {
            0.0
        } else {
            self.predicted_n as f64 * 1000.0 / self.predicted_ms as f64
        }
    }
}

pub fn timings_of(reply: &Value) -> Timings {
    let t = &reply["timings"];
    let n = |k: &str| t[k].as_u64().unwrap_or(0);
    let ms = |k: &str| t[k].as_f64().filter(|v| v.is_finite() && *v >= 0.0).map(|v| v.round() as u64).unwrap_or(0);
    Timings { prompt_n: n("prompt_n"), cache_n: n("cache_n"), predicted_n: n("predicted_n"), prompt_ms: ms("prompt_ms"), predicted_ms: ms("predicted_ms") }
}

/// What the model server answered: the text, and whether it was cut off at the token limit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    pub timings: Timings,
    pub text: String,
    pub cut_off: bool,
    /// Prompt tokens the server had to read for this request, and tokens it took from its cache.
    pub prompt_tokens_read: u64,
    pub prompt_tokens_cached: u64,
    /// When the request asked for them (a decision call): the tokens the model considered for its first word and how likely each was (0 to 1, the model's own, before
    /// anything held it to a grammar), most likely first. Empty otherwise.
    pub first_token_probs: Vec<(String, f64)>,
}

/// The probabilities of the tokens the model considered for the first word of its answer, from a reply to a request that asked for them (the shape of the
/// chat-completions endpoint, and the server's own), most likely first. Empty when the reply carries none.
pub fn top_probs_of(v: &Value) -> Vec<(String, f64)> {
    let listed = v.pointer("/choices/0/logprobs/content/0/top_logprobs").or_else(|| v.pointer("/completion_probabilities/0/top_logprobs")).and_then(Value::as_array);
    let mut out: Vec<(String, f64)> = match listed {
        Some(list) => list.iter().filter_map(|e| Some((e.get("token")?.as_str()?.to_string(), e.get("logprob")?.as_f64()?.exp()))).collect(),
        None => v
            .pointer("/completion_probabilities/0/probs")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(|e| Some((e.get("tok_str")?.as_str()?.to_string(), e.get("prob")?.as_f64()?))).collect())
            .unwrap_or_default(),
    };
    out.retain(|(_, p)| p.is_finite() && *p >= 0.0);
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// What the model server measured for this request, for the log: " - generated 87 tokens at 4.6/s, read at 36/s".
pub(super) fn timings_note(reply: &Value) -> String {
    let t = &reply["timings"];
    let (Some(n), Some(per_second)) = (t["predicted_n"].as_u64(), t["predicted_per_second"].as_f64()) else {
        return String::new();
    };
    match (t["prompt_n"].as_u64(), t["prompt_per_second"].as_f64()) {
        // A prompt read almost entirely from the cache has nothing to measure.
        (Some(read), Some(read_per_second)) if read >= 20 => format!(" - generated {n} tokens at {per_second:.1}/s, read at {read_per_second:.0}/s"),
        _ => format!(" - generated {n} tokens at {per_second:.1}/s"),
    }
}
