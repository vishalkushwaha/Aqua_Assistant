//! The words of a model call: who is asked (`ModelRef`, `Role`), what is asked (`ChatRequest`), what comes back (`ChatEvent`, `ChatOutcome`).
//!
//! A request is built by the caller and handed to a provider as it is. Nothing here knows about llama.cpp: how a request becomes bytes is the provider's business (`LlamaServerProvider`
//! writes the body the engine has always sent, byte for byte).

use super::error::ProviderError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// The name of a provider: `llama.cpp`, later `ollama`, `openai-compat`...
pub type ProviderId = String;

/// One model of one provider: `llama.cpp/Qwen3-8B-Q4_K_M.gguf`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: ProviderId,
    pub model: String,
}

impl ModelRef {
    pub fn new(provider: impl Into<ProviderId>, model: impl Into<String>) -> Self {
        Self { provider: provider.into(), model: model.into() }
    }

    /// `provider/model`, split at the first `/` (a model name may hold more of them). Both parts must be there.
    pub fn parse(text: &str) -> Option<Self> {
        let (provider, model) = text.trim().split_once('/')?;
        let (provider, model) = (provider.trim(), model.trim());
        (!provider.is_empty() && !model.is_empty()).then(|| Self::new(provider, model))
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

/// What a call is for. The router looks the model up by role (`RoleTable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The agent's planner.
    Agent,
    /// Plain chat, spoken to the person.
    Chat,
    /// Edits of code and text.
    Edit,
    /// Small utility calls (a title, a rewrite).
    Fast,
    /// A closed-set decision read from the first-token probabilities.
    Decision,
    /// Triage and the vote that checks an answer.
    Diagnosis,
    /// Summaries.
    Summary,
}

impl Role {
    pub const ALL: [Role; 7] = [Role::Agent, Role::Chat, Role::Edit, Role::Fast, Role::Decision, Role::Diagnosis, Role::Summary];

    pub fn name(self) -> &'static str {
        match self {
            Role::Agent => "agent",
            Role::Chat => "chat",
            Role::Edit => "edit",
            Role::Fast => "fast",
            Role::Decision => "decision",
            Role::Diagnosis => "diagnosis",
            Role::Summary => "summary",
        }
    }

    pub fn parse(text: &str) -> Option<Role> {
        let word = text.trim().to_ascii_lowercase();
        Role::ALL.into_iter().find(|r| r.name() == word)
    }
}

/// The kind of work a request is, for the logs and for a provider that treats kinds differently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Purpose {
    #[default]
    Chat,
    Planner,
    Edit,
    Summary,
    Decision,
    Diagnosis,
    Reader,
    /// A read of a prompt start into the cache, one token written and thrown away.
    Warmup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

impl Message {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self { role, content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(MessageRole::User, content)
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(MessageRole::System, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(MessageRole::Assistant, content)
    }
}

/// A tool described to a provider that takes tools natively. (The local model does not: Aqua's planner names its tool in JSON held to a schema, see [`Output::JsonSchema`].)
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// A JSON schema of the arguments.
    pub parameters: Value,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Named(String),
}

/// What the reply must look like, enforced by the server while it decodes.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Output {
    /// Free text.
    #[default]
    Text,
    /// JSON that fits this schema (the planner's reply).
    JsonSchema(Value),
    /// Text that matches this GBNF grammar.
    Grammar(String),
    /// One of these words and nothing else, with the probabilities the model gave to the first token (a decision). The caller asks for one token.
    Choice(Vec<String>),
}

/// Sampling settings. Every field is optional: what is not set is what the provider has always used (the local server: temperature 0.3, top_p 0.9).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub seed: Option<u64>,
    pub stop: Vec<String>,
    pub repeat_penalty: Option<f64>,
}

impl Sampling {
    /// Nothing set: the provider's own numbers.
    pub fn is_empty(&self) -> bool {
        *self == Sampling::default()
    }
}

/// Whether the model may think before it answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReasoningMode {
    /// What the running server was started with (thinking is off unless `AQUA_LLAMA_THINK_BUDGET` is above 0).
    #[default]
    ServerDefault,
    Off,
    On,
    /// The model's chat template has no thinking switch, so none is sent (no `chat_template_kwargs` at all): a model of a family that does not think, or thinks by its own rules (card OA9-03).
    NoSwitch,
}

/// Where the provider may keep what it has read of the prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheHint {
    /// The server's cache slot (`id_slot`), when it has one for this kind of request.
    pub slot: Option<u32>,
    /// A key for a provider whose cache is keyed (none today).
    pub key: Option<String>,
}

/// One request. Borrowed from the caller; a provider copies what it needs.
#[derive(Clone, Debug)]
pub struct ChatRequest<'a> {
    /// Instructions, the stable part first (it is the start of the prompt the server's cache keeps).
    pub system: &'a [&'a str],
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
    pub tool_choice: ToolChoice,
    pub output: Output,
    pub sampling: Sampling,
    /// The most tokens the reply may take.
    pub max_tokens: u32,
    pub reasoning: ReasoningMode,
    pub cache: CacheHint,
    pub purpose: Purpose,
    /// Hand the text over as it is written (`TextDelta` events a few characters at a time) instead of in one piece at the end.
    pub stream: bool,
}

impl<'a> ChatRequest<'a> {
    /// A request for free text with the provider's own sampling, no tools and no cache hint.
    pub fn new(messages: &'a [Message], max_tokens: u32, purpose: Purpose) -> Self {
        Self {
            system: &[],
            messages,
            tools: &[],
            tool_choice: ToolChoice::Auto,
            output: Output::Text,
            sampling: Sampling::default(),
            max_tokens,
            reasoning: ReasoningMode::ServerDefault,
            cache: CacheHint::default(),
            purpose,
            stream: false,
        }
    }

    pub fn with_output(mut self, output: Output) -> Self {
        self.output = output;
        self
    }

    pub fn with_slot(mut self, slot: Option<u32>) -> Self {
        self.cache.slot = slot;
        self
    }

    pub fn with_sampling(mut self, sampling: Sampling) -> Self {
        self.sampling = sampling;
        self
    }

    pub fn with_reasoning(mut self, reasoning: ReasoningMode) -> Self {
        self.reasoning = reasoning;
        self
    }

    pub fn with_system(mut self, system: &'a [&'a str]) -> Self {
        self.system = system;
        self
    }

    pub fn streaming(mut self) -> Self {
        self.stream = true;
        self
    }

    /// The prompt as one text, for a provider that takes one: the system parts and the messages in order, separated by a blank line. A single user message with no system part comes
    /// out as it went in, which is what the engine sends today. A message of another role is labelled with its role so that it is not taken for the person's words.
    pub fn flattened(&self) -> String {
        let mut parts: Vec<String> = self.system.iter().map(|s| (*s).to_string()).collect();
        for m in self.messages {
            parts.push(match m.role {
                MessageRole::User => m.content.clone(),
                MessageRole::System => m.content.clone(),
                MessageRole::Assistant => format!("Assistant: {}", m.content),
                MessageRole::Tool => format!("Tool result: {}", m.content),
            });
        }
        parts.join("\n\n")
    }
}

/// A flag that nobody sets, for a call that only the person's Stop (seen by the provider itself) can end.
static NEVER: AtomicBool = AtomicBool::new(false);

/// What a caller does about a prompt the provider refused as too long: it is given the refusal and returns the messages of the same turn with less in them, or `None` when nothing
/// more can be taken out. The router calls it at most once for a call (card OA9-02).
pub type Shrink<'a> = &'a mut dyn FnMut(&ProviderError) -> Option<Vec<Message>>;

/// How a call is run.
pub struct CallControl<'a> {
    /// Set by the caller to hang up. The provider checks it as often as it looks at anything else.
    pub cancel: &'a AtomicBool,
    /// The most time the whole call may take.
    pub deadline: Duration,
    /// How long a streamed answer may go quiet, once its first event has come, before the call is hung up on and ends as `ProviderError::Timeout { idle: true }`. `None`: the provider's
    /// own setting (the local one reads `AQUA_STREAM_IDLE_SECS`, which is off unless it is set). It is not armed before the first event, so a prompt that takes a while to read is not
    /// taken for a stall, and a call that is not streamed has no events to wait for.
    pub idle: Option<Duration>,
    /// How long a streamed call may wait for its first event (the server has read the prompt and begun to answer). `None`: the provider's own setting (`AQUA_STREAM_FIRST_EVENT_SECS`).
    pub first_event: Option<Duration>,
    /// Told how many seconds the call has been waiting or working, about once a second.
    pub on_tick: Option<&'a mut dyn FnMut(u64)>,
    /// What to do about a prompt that does not fit: see [`Shrink`]. `None`: the refusal goes back to the caller.
    pub shrink: Option<Shrink<'a>>,
}

impl<'a> CallControl<'a> {
    pub fn new(cancel: &'a AtomicBool, deadline: Duration) -> Self {
        Self { cancel, deadline, idle: None, first_event: None, on_tick: None, shrink: None }
    }

    /// No flag of its own: only the person's Stop (which the local provider sees for itself) ends the call early.
    pub fn with_deadline(deadline: Duration) -> Self {
        CallControl { cancel: &NEVER, deadline, idle: None, first_event: None, on_tick: None, shrink: None }
    }

    pub fn with_ticks(mut self, on_tick: &'a mut dyn FnMut(u64)) -> Self {
        self.on_tick = Some(on_tick);
        self
    }

    pub fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = Some(idle);
        self
    }

    pub fn with_first_event(mut self, first_event: Duration) -> Self {
        self.first_event = Some(first_event);
        self
    }

    pub fn with_shrink(mut self, shrink: Shrink<'a>) -> Self {
        self.shrink = Some(shrink);
        self
    }

    /// Tell the listener that `secs` seconds have passed.
    pub fn tick(&mut self, secs: u64) {
        if let Some(f) = self.on_tick.as_mut() {
            f(secs);
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// What a provider reports while a call runs.
#[derive(Clone, Debug, PartialEq)]
pub enum ChatEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, json_fragment: String },
    ToolCall { id: String, name: String, arguments: Value },
    Usage(Usage),
    Finish(FinishReason),
}

/// What the listener of a stream says after each event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Nobody is listening any more: hang up (the call ends as `ProviderError::Cancelled`).
    Stop,
}

/// What a call cost, as the server measured it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Prompt tokens the server had to read.
    pub input: u64,
    /// Prompt tokens it took from its cache.
    pub cached: u64,
    /// Tokens it wrote.
    pub output: u64,
    pub reasoning: Option<u64>,
    /// Milliseconds spent reading the prompt (the wait before the first token) and writing the answer.
    pub prompt_ms: u64,
    pub decode_ms: u64,
    /// Only a remote provider has a price.
    pub cost_usd: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    /// Ran out of tokens in the middle of the reply.
    Length,
    ToolCalls,
    ContentFilter,
    Cancelled,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// The whole of a finished call.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatOutcome {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish: FinishReason,
    pub model: ModelRef,
    /// For [`Output::Choice`]: the probabilities of the tokens the model considered for its first word, most likely first. Empty otherwise.
    pub choice_probs: Vec<(String, f64)>,
}

impl ChatOutcome {
    /// Did the model run out of tokens before it finished?
    pub fn cut_off(&self) -> bool {
        self.finish == FinishReason::Length
    }
}
