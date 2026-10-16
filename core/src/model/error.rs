//! Why a model call did not give an answer, in classes a caller can decide on.
//!
//! The local server speaks in `llama_server::Failure` (four cases, each with its message). This is the wider set a provider of any kind uses, and the conversion in both directions
//! keeps the message of each case word for word, so that a caller that still reads the old type sees the text it always saw. The classes drive policy: what may be asked again
//! ([`ProviderError::retryable`]), after how long ([`ProviderError::retry_after`]), what must never be asked again unchanged (a [`ProviderError::ContextOverflow`]). The policy that
//! asks again is in `retry.rs`, and the router applies it (card OA9-02). [`classify_http`] gives the class of an error reply of a service over a network, from its status, its headers
//! and the words of its body, and never repeats those words.

use super::retry::retry_after_from_headers;
use crate::modules::llm_router::llama_server::{self, Failure};
use std::fmt;
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, PartialEq)]
pub enum ProviderError {
    /// The server or service cannot be used (missing, would not start, crashed). The caller may fall back to another way.
    Unavailable { detail: String },
    RateLimited { retry_after: Option<Duration> },
    Overloaded { retry_after: Option<Duration> },
    /// The prompt does not fit the window. Asking again unchanged fails again; the caller shrinks the prompt first.
    ContextOverflow { limit: Option<u32>, detail: String },
    /// Any other 4xx: the provider is fine and refused this request.
    Rejected(String),
    Auth(String),
    Quota(String),
    ContentPolicy(String),
    /// Out of time. `idle`: the stream went quiet (the watchdog of the local provider, `AQUA_STREAM_IDLE_SECS`), as against the whole call running past its time.
    Timeout { idle: bool, detail: String },
    /// The turn was stopped (the person's Stop, or a listener that left).
    Cancelled,
    /// The call gave the model up to a person who was waiting. Not a failure: the caller asks again at the next step.
    Yielded,
    /// The provider answered with something that cannot be read.
    InvalidOutput(String),
    Transport(String),
}

impl ProviderError {
    /// May the same request be asked again, unchanged, with a fair hope? A provider that is down is not retried (the caller falls back); a class of refusal is not retried either.
    pub fn retryable(&self) -> bool {
        matches!(self, ProviderError::RateLimited { .. } | ProviderError::Overloaded { .. } | ProviderError::Transport(_))
    }

    /// How long the provider asked to wait, when it said.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ProviderError::RateLimited { retry_after } | ProviderError::Overloaded { retry_after } => *retry_after,
            _ => None,
        }
    }

    /// Is this the person's Stop (or a listener that left)?
    pub fn is_cancelled(&self) -> bool {
        matches!(self, ProviderError::Cancelled)
    }

    /// A short word for the class, for logs and metrics labels.
    pub fn class(&self) -> &'static str {
        match self {
            ProviderError::Unavailable { .. } => "unavailable",
            ProviderError::RateLimited { .. } => "rate_limited",
            ProviderError::Overloaded { .. } => "overloaded",
            ProviderError::ContextOverflow { .. } => "context_overflow",
            ProviderError::Rejected(_) => "rejected",
            ProviderError::Auth(_) => "auth",
            ProviderError::Quota(_) => "quota",
            ProviderError::ContentPolicy(_) => "content_policy",
            ProviderError::Timeout { .. } => "timeout",
            ProviderError::Cancelled => "cancelled",
            ProviderError::Yielded => "yielded",
            ProviderError::InvalidOutput(_) => "invalid_output",
            ProviderError::Transport(_) => "transport",
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Unavailable { detail } | ProviderError::ContextOverflow { detail, .. } | ProviderError::Timeout { detail, .. } => f.write_str(detail),
            ProviderError::Rejected(m) | ProviderError::Auth(m) | ProviderError::Quota(m) | ProviderError::ContentPolicy(m) | ProviderError::InvalidOutput(m) | ProviderError::Transport(m) => f.write_str(m),
            ProviderError::RateLimited { retry_after } => write!(f, "the model service is limiting requests{}", wait_note(*retry_after)),
            ProviderError::Overloaded { retry_after } => write!(f, "the model service is overloaded{}", wait_note(*retry_after)),
            // The text the engine has always given for a stopped turn.
            ProviderError::Cancelled => f.write_str("stopped: the task was cancelled"),
            ProviderError::Yielded => f.write_str(&llama_server::yielded_message()),
        }
    }
}

fn wait_note(after: Option<Duration>) -> String {
    after.map(|d| format!(" (it asks to wait {} s)", d.as_secs())).unwrap_or_default()
}

impl std::error::Error for ProviderError {}

/// Does this refusal text say the prompt did not fit? `Some(limit)` when it does (the limit when the text names it). The phrases are llama.cpp's own ("exceeds the available context
/// size (2048 tokens)", "the prompt has N tokens, but the configured context size is M"), the ones the other servers and services use (the list OpenCode keeps in
/// `provider-error.ts`: Anthropic, Bedrock, OpenAI, Google, xAI, Groq, OpenRouter, DeepSeek, Copilot, LM Studio, MiniMax, Kimi), the status text of a request that is too large (413),
/// and Aqua's own guard ("too much text").
pub fn context_overflow(message: &str) -> Option<Option<u32>> {
    let lower = message.to_ascii_lowercase();
    const PHRASES: &[&str] = &[
        // llama.cpp, LM Studio, the common wording.
        "exceeds the available context size",
        "exceeds the context size",
        "configured context size",
        "context_length_exceeded",
        "maximum context length",
        "exceeds the context window",
        "greater than the context length",
        // Anthropic, Bedrock, xAI, Groq, MiniMax, Kimi.
        "prompt is too long",
        "input is too long for requested model",
        "maximum prompt length is",
        "reduce the length of the messages",
        "context window exceeds limit",
        "exceeded model token limit",
        // A request too large for the service to read at all (HTTP 413).
        "request entity too large",
        // Aqua's own guard against a runaway paste.
        "too much text for the language model",
    ];
    // Two phrases that are only an overflow with a second word: Google's "input token count ... exceeds the maximum number of tokens", Copilot's "token count of N exceeds the limit of M".
    let with_second_word = (lower.contains("input token count") && lower.contains("exceeds the maximum")) || (lower.contains("exceeds the limit of") && lower.contains("token"));
    if !with_second_word && !PHRASES.iter().any(|p| lower.contains(p)) {
        return None;
    }
    // The limit is the first number after the words "context size" / "context length" / "prompt length" ... (either "(2048 tokens)" or "is 2048").
    let limit = ["context size", "context length", "prompt length", "context window", "maximum number of tokens", "limit of"].iter().find_map(|marker| {
        let at = lower.find(marker)? + marker.len();
        let digits: String = lower[at..].chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    Some(limit)
}

/// The class of a refusal by its text: an overflow when it says so, any other refusal otherwise.
pub fn classify_rejection(message: String) -> ProviderError {
    match context_overflow(&message) {
        Some(limit) => ProviderError::ContextOverflow { limit, detail: message },
        None => ProviderError::Rejected(message),
    }
}

impl ProviderError {
    /// The class of an error that reached a caller only as text (the `Result<String, String>` of the old traits). The text is kept as it is; the class is found by the sentences
    /// the engine writes: a call that gave the model up to a person, a stopped task, a prompt that does not fit, a request the server refused, a time-out. Anything else is
    /// taken as a model that could not be used.
    pub fn from_text(text: &str) -> ProviderError {
        let message = text.to_string();
        if llama_server::is_yield_text(text) {
            ProviderError::Yielded
        } else if text.starts_with("stopped") {
            ProviderError::Cancelled
        } else if context_overflow(text).is_some() {
            classify_rejection(message)
        } else if text.contains("the language model refused this request") {
            ProviderError::Rejected(message)
        } else if llama_server::is_idle_text(text) {
            ProviderError::Timeout { idle: true, detail: message }
        } else if text.contains("timed out") {
            ProviderError::Timeout { idle: false, detail: message }
        } else {
            ProviderError::Unavailable { detail: message }
        }
    }

    /// The same for an `anyhow` error: a `ProviderError` inside it is the answer, a refused request (`RequestRefused`) is a refusal, anything else is read by its text.
    pub fn from_anyhow(error: &anyhow::Error) -> ProviderError {
        if let Some(typed) = error.downcast_ref::<ProviderError>() {
            return typed.clone();
        }
        if let Some(refused) = error.downcast_ref::<crate::modules::llm_router::llama_engine::RequestRefused>() {
            return classify_rejection(refused.0.clone());
        }
        ProviderError::from_text(&format!("{error:#}"))
    }
}

/// The class of an error reply of a service over a network (a service on another machine, an Ollama that is overloaded), from its status, its headers and the words of its body.
///
/// * 413, and a body that says the prompt does not fit: [`ProviderError::ContextOverflow`], with the limit when the body names it.
/// * 402, and a body that says the account has no credit or quota left: [`ProviderError::Quota`] (before 429, because a spent quota is not "later").
/// * 401 and 403 (and a body that says the key is wrong): [`ProviderError::Auth`]; a body that says the content was refused: [`ProviderError::ContentPolicy`].
/// * 429: [`ProviderError::RateLimited`]; 500, 502, 503, 504 and 529: [`ProviderError::Overloaded`]; 408: [`ProviderError::Transport`]. The first two carry the wait the service asked
///   for in `Retry-After` or `Retry-After-Ms` ([`retry_after_from_headers`]).
/// * any other 4xx: [`ProviderError::Rejected`].
///
/// **What the service wrote is read and not repeated.** The message is made here, from the class, the status and, when the body is JSON with a short `error.code` or `error.type`
/// (letters, digits, `_`, `-`, `.`, at most 60), that word. A body can echo the prompt or carry anything, and the error goes to logs, to the screen and into the model's context.
pub fn classify_http(status: u16, headers: &[(String, String)], body: &str, now: SystemTime) -> ProviderError {
    let head: String = body.chars().take(4000).collect();
    let lower = head.to_ascii_lowercase();
    let wait = retry_after_from_headers(headers.iter().map(|(n, v)| (n.as_str(), v.as_str())), now);
    let code = error_code(&head);
    let tag = match &code {
        Some(code) => format!("HTTP {status}, {code}"),
        None => format!("HTTP {status}"),
    };
    let says = |words: &[&str]| words.iter().any(|w| lower.contains(w));

    if let Some(limit) = context_overflow(&head) {
        let named = limit.map(|n| format!(" ({n} tokens)")).unwrap_or_default();
        return ProviderError::ContextOverflow { limit, detail: format!("the model service refused the request because it does not fit its context window{named} ({tag})") };
    }
    if status == 413 {
        return ProviderError::ContextOverflow { limit: None, detail: format!("the model service refused the request because it is too large ({tag})") };
    }
    if status == 402 || says(&["insufficient_quota", "exceeded your current quota", "credit balance is too low", "out of credits", "billing_hard_limit", "insufficient credits", "payment required"]) {
        return ProviderError::Quota(format!("the account at the model service has no quota or credit left ({tag})"));
    }
    if says(&["content_policy", "content policy", "content_filter", "content filter", "usage policy", "safety system"]) && status != 429 && status < 500 {
        return ProviderError::ContentPolicy(format!("the model service refused the request because of its content rules ({tag})"));
    }
    if matches!(status, 401 | 403) || (status < 500 && says(&["invalid api key", "invalid_api_key", "incorrect api key", "authentication_error", "unauthorized"])) {
        return ProviderError::Auth(format!("the model service did not accept the key ({tag})"));
    }
    match status {
        429 => ProviderError::RateLimited { retry_after: wait },
        408 => ProviderError::Transport(format!("the model service did not get the whole request in time ({tag})")),
        500..=599 => ProviderError::Overloaded { retry_after: wait },
        _ if says(&["rate_limit", "rate limit", "too many requests"]) => ProviderError::RateLimited { retry_after: wait },
        _ if says(&["overloaded"]) => ProviderError::Overloaded { retry_after: wait },
        _ => ProviderError::Rejected(format!("the model service refused the request ({tag})")),
    }
}

/// The short word a JSON error body names its kind by (`error.code`, else `error.type`, else the same two at the top), when it is made of safe characters only.
fn error_code(body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let word = |v: &serde_json::Value| v.as_str().filter(|w| !w.is_empty() && w.len() <= 60 && w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))).map(str::to_string);
    ["/error/code", "/error/type", "/code", "/type"].iter().find_map(|p| json.pointer(p).and_then(word))
}

impl From<Failure> for ProviderError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Timeout(message) if llama_server::is_yield_text(&message) => ProviderError::Yielded,
            Failure::Timeout(detail) => ProviderError::Timeout { idle: llama_server::is_idle_text(&detail), detail },
            Failure::Unavailable(detail) => ProviderError::Unavailable { detail },
            Failure::Rejected(message) => classify_rejection(message),
            Failure::Cancelled => ProviderError::Cancelled,
        }
    }
}

/// Back to what the engine reads. A class the local server cannot produce is told as the nearest case: a refusal of the request, or a server that cannot be used.
impl From<ProviderError> for Failure {
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::Yielded => llama_server::yielded_failure(),
            ProviderError::Timeout { detail, .. } => Failure::Timeout(detail),
            ProviderError::Cancelled => Failure::Cancelled,
            ProviderError::Unavailable { detail } => Failure::Unavailable(detail),
            ProviderError::ContextOverflow { detail, .. } => Failure::Rejected(detail),
            ProviderError::Rejected(m) | ProviderError::Auth(m) | ProviderError::Quota(m) | ProviderError::ContentPolicy(m) => Failure::Rejected(m),
            other @ (ProviderError::RateLimited { .. } | ProviderError::Overloaded { .. } | ProviderError::InvalidOutput(_) | ProviderError::Transport(_)) => Failure::Unavailable(other.to_string()),
        }
    }
}
