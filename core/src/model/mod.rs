//! One seam for the language model (card OA9-01; tasks PR-02 and AQ-10 of the OpenCode audit).
//!
//! Before this there were three ways to ask the model for text, and two more that go elsewhere: the agent's `Planner`, the flows' `Completer`, and the engine called directly by the
//! pipeline, the decision router and the warm-up. Each had its own error type (`String`, `anyhow`, the server's four-case `Failure`), none could be tested with a scripted model
//! except the agent loop, and the engine's type was named in 18 files.
//!
//! What is here:
//!
//! * [`types`]: the words of a call: [`ModelRef`], [`Role`], [`ChatRequest`] (system parts, messages, an [`Output`] that the reply is held to, [`Sampling`], the cache slot),
//!   [`CallControl`], the [`ChatEvent`]s a call reports and the [`ChatOutcome`].
//! * [`error`]: [`ProviderError`], the classes a caller decides on, converted to and from the server's `Failure` without losing its message.
//! * [`ModelProvider`] (this file): `chat`, `list`, `health`, `count_tokens`, `warm`. [`LlamaServerProvider`] is the one real provider (the local llama.cpp server, which keeps its
//!   process, lock, GPU plan and cache slots exactly as they were) and [`MockProvider`] is a scripted one for tests: any turn can be tried with a model that says what the test wants.
//! * [`ModelRouter`]: a [`RoleTable`] of which model answers which role, and the call that finds the provider and runs it. Before each call it asks its [`EgressGate`] (card OA9-04):
//!   what is on this machine may be asked, and a provider on another machine is refused with the reason (the gate that would clean the text, ask the person and cap the spend is not built).
//! * [`text_model`]: [`TextModel`], one trait for "a prompt in, text out" with [`ProviderError`] as its error, and adapters in both directions for the flows' `Completer`, the agent's
//!   `Planner`, the reader's `ReadModel` and the dictation `Rewriter` and `Normaliser`, which stay as they are.
//! * [`gguf`], [`profile`], [`roles`], [`sampling`], [`adapter`] (card OA9-03): what a model file says about itself, the person's `profiles.json` and `router.json`, the layers of
//!   sampling, and how each family is spoken to. See `documentation/MODEL_PROFILES.md`.
//! * [`ledger`] (card OA9-05): what each planner reply of a run cost, as the model server measured it, in the words of the run record (`tokens_in`, `cache_read`, `tokens_out`), summed by
//!   run and by model, with no text in it. Read by `GET /api/v1/runs/{id}/usage`.
//! * `cassette` (card OA9-05, tests only): recorded exchanges of a model service in `core/tests/fixtures/provider/`, replayed with no network, through the code the engine reads the
//!   local server with and through the router's retry policy. See `documentation/MODEL_PROVIDER.md`.
//! * [`openai_compat`], [`openai_wire`], [`sse`], [`providers_config`], [`egress`] (card OA9-04): [`OpenAiCompatProvider`] is an Ollama, an LM Studio or another OpenAI-style server **on this
//!   machine** as a provider, listed in `data/models/providers.json`; `openai_wire` writes its requests and reads its answers; `sse` is the chunked and event reading it shares with the
//!   local server; `egress` is the gate. Off unless `AQUA_MODEL_PROVIDERS` is on, and nothing in the engine asks it: the engine's calls go to the local server as they always did. See
//!   `documentation/MODEL_PROVIDER.md`.
//! * [`bridge`]: the five places where the engine asks the server go through here. With `AQUA_MODEL_ROUTER` off (the default) the bridge calls the server exactly as the engine always
//!   did. With it on the request is described as a [`ChatRequest`] and goes through the router to [`LlamaServerProvider`], which sends the same bytes (a test compares them with a golden
//!   body).
//! * [`native_tools`] (card OA9-06, task PR-14): the server's own tool calls as a selectable mode of the agent's planner (`AQUA_PLANNER_TOOL_MODE` or `tool_mode` in a profile; json,
//!   the constrained reply, is the default): the offer made from the round's tools, and the reading of a reply's `tool_calls` back into the text the loop has always read, so a call
//!   still goes through the loop and `ToolHost::call` as the same call. Nothing is asked in this mode unless a bake-off says it is as accurate. See `documentation/SPEED_EXPERIMENTS.md`.
//! * [`fast_tier`] (card OA9-06, task PR-11): the routing decision of a small second model for the roles decision, fast and diagnosis, and the fall back to the main model when it is missing,
//!   too slow, busy or too small (`AQUA_FAST_TIER`, off), tested with scripted models.
//! * [`fast_route`] (card OA9-06, second round): the wiring of that rule to the engine, behind the same switch. The bridge asks it, for the roles decision and fast, which tier answers; with the
//!   switch off it says none and nothing is read or started. The small model is answered by a second model server of its own (`modules::llm_router::fast_server`).
//!
//! **What does not change.** The prompt text, `/no_think`, the removal of thinking tags, the retry of a cut-off reply with twice the room, the fall back to `llama-cli`, the
//! second try without a constraint, the cache slots, the model chosen for the agent and for chat: all of it stays in `LlamaRouterEngine` for now, because those are the policy
//! the next cards (OA9-02 to OA9-04) turn into per-model profiles. The provider is the part that talks to the server.

pub mod adapter;
pub mod bridge;
#[cfg(any(test, feature = "test-support"))]
pub mod cassette;
pub mod egress;
pub mod error;
pub mod fast_route;
pub mod fast_tier;
pub mod gguf;
pub mod ledger;
pub mod llama_server_provider;
pub mod mock;
pub mod native_tools;
pub mod openai_compat;
pub mod openai_wire;
pub mod profile;
pub mod providers_config;
pub mod retry;
pub mod roles;
pub mod router;
pub mod sampling;
pub mod sse;
pub mod text_model;
pub mod types;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod profile_tests;

#[cfg(test)]
mod openai_compat_tests;

#[cfg(test)]
mod native_tools_tests;

#[cfg(test)]
mod fast_tier_tests;

pub use egress::{EgressGate, LocalOnlyGate};
pub use error::ProviderError;
pub use llama_server_provider::LlamaServerProvider;
pub use mock::{MockProvider, Script};
pub use openai_compat::{Endpoint, ModelSpec, OpenAiCompatProvider, ProviderSpec};
pub use openai_wire::Shape;
pub use retry::RetryPolicy;
pub use router::{set_counter_hook, CounterHook, ModelRouter, RoleTable};
pub use text_model::{LlmError, TextModel, TextReply, TextRequest};
pub use types::*;

use crate::flags;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

/// Is the model router on (`AQUA_MODEL_ROUTER`, off unless it says on)? Off, the engine asks the server directly, as it always has.
pub fn router_enabled() -> bool {
    flags::is_on(&flags::MODEL_ROUTER)
}

/// Where a provider's models run, which decides whether a call needs the person's say-so (a remote one does; the gate that asks is not built, so the router's gate refuses it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Egress {
    /// On this machine, in a process Aqua started.
    Local,
    /// A server on this machine that Aqua did not start (an Ollama, an LM Studio).
    Loopback,
    /// Another machine.
    Remote { host: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Unknown,
    /// Nothing is loaded; the first call starts it.
    Stopped,
    /// Loaded and answering.
    Ready,
}

/// The family a model belongs to; it decides which quirks a model needs (a later card).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Family {
    Qwen3,
    Qwen3Coder,
    Llama3,
    Gemma3,
    Mistral,
    Phi,
    Granite,
    Other(String),
}

impl Family {
    /// From a GGUF file name ("Qwen3-8B-Q4_K_M.gguf"), by the words the common families put in theirs. Anything else is `Other` with the name's first word.
    pub fn from_filename(name: &str) -> Family {
        let lower = name.to_ascii_lowercase();
        let has = |w: &str| lower.contains(w);
        if has("qwen3-coder") || has("qwen3_coder") {
            Family::Qwen3Coder
        } else if has("qwen3") {
            Family::Qwen3
        } else if has("llama-3") || has("llama3") || has("llama_3") {
            Family::Llama3
        } else if has("gemma-3") || has("gemma3") {
            Family::Gemma3
        } else if has("mistral") {
            Family::Mistral
        } else if has("phi-") || has("phi3") || has("phi4") {
            Family::Phi
        } else if has("granite") {
            Family::Granite
        } else {
            Family::Other(lower.split(['-', '_', '.', ' ']).next().unwrap_or("").to_string())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolMode {
    None,
    /// The reply is JSON held to a schema that names the tool (Aqua's planner).
    ConstrainedJson,
    /// The provider takes tool definitions and returns tool calls.
    Native,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuredMode {
    None,
    JsonObject,
    JsonSchema,
    /// A GBNF grammar (and so also a JSON schema, which the server turns into one).
    Gbnf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningSupport {
    None,
    /// Thinking can be switched on or off per request.
    Toggle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptCache {
    None,
    /// The server keeps what it has read in this many slots.
    LocalKv { slots: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub tools: ToolMode,
    pub structured: StructuredMode,
    pub reasoning: ReasoningSupport,
    pub vision: bool,
    /// Can give the probabilities of the first token (a decision needs them).
    pub logprobs: bool,
    pub seed: bool,
    pub prompt_cache: PromptCache,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Tokens one request can use (the window divided between the cache slots).
    pub context: u32,
    /// The most tokens one reply is given.
    pub max_output: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Residency {
    Local { bytes: u64, quant: Option<String> },
    Remote { host: String },
}

/// What a provider says about one of its models.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub model: ModelRef,
    pub display_name: String,
    pub family: Family,
    pub caps: Capabilities,
    pub limits: Limits,
    pub residency: Residency,
}

/// Something that answers chat requests: the local server, a scripted model, later an Ollama or a remote service.
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> &str;

    fn egress(&self) -> Egress;

    /// The models this provider can answer with right now.
    fn list(&self) -> Vec<ModelInfo>;

    /// One call. Events are handed to `sink` as they happen. In a streamed call (`ChatRequest::stream`) a sink that returns [`Flow::Stop`] hangs up and the call ends as
    /// [`ProviderError::Cancelled`]; a call that is not streamed hands its text over once the answer is complete, and Stop then has nothing to hang up. The returned outcome has the whole
    /// text either way. An error after some text was handed over is final: the caller must not ask again as if nothing had been said.
    fn chat(&self, model: &str, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError>;

    /// How many tokens `text` is for this model, when the provider can say without a call.
    fn count_tokens(&self, _model: &str, _text: &str) -> Option<u32> {
        None
    }

    /// Have the provider read `prefix` now, so that the request that starts with it finds it in its cache: a call that writes one token and throws it away.
    fn warm(&self, model: &str, prefix: &str, slot: Option<u32>) -> Result<(), ProviderError> {
        let messages = [Message::user(prefix)];
        let req = ChatRequest::new(&messages, 1, Purpose::Warmup).with_slot(slot);
        let mut ctl = CallControl::with_deadline(std::time::Duration::from_secs(300));
        self.chat(model, &req, &mut ctl, &mut |_| Flow::Continue).map(|_| ())
    }

    fn health(&self) -> Health {
        Health::Unknown
    }
}

/// Is the OpenAI-compatible provider on (`AQUA_MODEL_PROVIDERS`, off unless it says on)? Off, `data/models/providers.json` is not read and no provider of it is made.
pub fn providers_enabled() -> bool {
    flags::is_on(&flags::MODEL_PROVIDERS)
}

/// The router the engine's calls go through when `AQUA_MODEL_ROUTER` is on: the local server and nothing else.
pub fn default_router() -> Arc<ModelRouter> {
    static ROUTER: OnceLock<Arc<ModelRouter>> = OnceLock::new();
    ROUTER.get_or_init(|| Arc::new(ModelRouter::new(vec![Arc::new(LlamaServerProvider::new(None))]))).clone()
}

#[cfg(test)]
thread_local! {
    static TEST_ROUTER: std::cell::RefCell<Option<Arc<ModelRouter>>> = const { std::cell::RefCell::new(None) };
}

/// The router a call on this thread goes through: in a test, the one given to [`with_test_router`]; otherwise [`default_router`].
pub(crate) fn current_router() -> Arc<ModelRouter> {
    #[cfg(test)]
    if let Some(router) = TEST_ROUTER.with(|r| r.borrow().clone()) {
        return router;
    }
    default_router()
}

/// Runs `f` with `router` as the router of this thread (tests): the engine's calls then go to a scripted model and no server is touched. Nothing is shared with other threads.
#[cfg(test)]
pub(crate) fn with_test_router<R>(router: Arc<ModelRouter>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<ModelRouter>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_ROUTER.with(|r| *r.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(TEST_ROUTER.with(|r| r.replace(Some(router))));
    f()
}
