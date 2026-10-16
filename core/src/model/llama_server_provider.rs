//! The local llama.cpp server as a [`ModelProvider`].
//!
//! This wraps what was already there and changes none of it: the one server process, its lock and queue, the GPU plan and the retry on the processor after a card failure, the
//! cache slots, the person's Stop, a background run giving the model up to a person (`Yielded`). A call is `llama_server::complete_inner`, and the body it sends is built by
//! `llama_server::build_body`, the same function the engine's own calls use, so a request described as a [`ChatRequest`] is the request the engine sent before there was one.
//! `egress()` is `Local`: nothing leaves the machine.

use super::error::ProviderError;
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, Flow, FinishReason, ModelRef, Output, Usage};
use super::{Capabilities, Egress, Family, Health, Limits, ModelInfo, ModelProvider, PromptCache, ReasoningSupport, Residency, StructuredMode, ToolMode};
use crate::modules::llm_router::llama_cli::{list_gguf_models, resolve_named_model, GgufModelInfo};
use crate::modules::llm_router::llama_server::{self as server, Constraint, Extras, Reply};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The provider's name in a [`ModelRef`].
pub const ID: &str = "llama.cpp";

/// The most tokens one reply of the local model is ever given (the planner's cap: a reply that is cut off is asked for again once with twice the room, never above this).
pub const MAX_OUTPUT_TOKENS: u32 = crate::modules::llm_router::llama_engine::MAX_REPLY_TOKENS;

pub struct LlamaServerProvider {
    /// The folder of GGUF files, for a model named by its file name. A model can also be named by the full path of a GGUF file (the engine has already chosen and found its file).
    model_dir: Option<PathBuf>,
}

impl LlamaServerProvider {
    pub fn new(model_dir: Option<PathBuf>) -> Self {
        Self { model_dir }
    }

    /// The file for `model`: a path to a file that exists (taken exactly as written, so that the server is asked for the same file the engine has always passed it, and a server
    /// already running for it is reused; the engine's own choice, `AQUA_LLAMA_MODEL` included, is not second-guessed), or a bare `.gguf` file name in the model folder.
    fn path_of(&self, model: &str) -> Result<PathBuf, ProviderError> {
        let named = Path::new(model);
        if named.is_absolute() || named.components().count() > 1 {
            return if named.is_file() { Ok(named.to_path_buf()) } else { Err(ProviderError::Unavailable { detail: format!("the model file {model} was not found") }) };
        }
        let Some(dir) = &self.model_dir else {
            return Err(ProviderError::Unavailable { detail: format!("the model {model} was named by file name, and this provider was given no model folder") });
        };
        resolve_named_model(dir, model).ok_or_else(|| ProviderError::Unavailable { detail: format!("the model file {model} was not found in the model folder") })
    }
}

/// How a request becomes the server's request: the prompt as one user message, the constraint, the probabilities to ask for, and the extra body fields.
struct Plan {
    prompt: String,
    constraint: Option<Constraint>,
    logprobs: Option<u32>,
    fields: Map<String, Value>,
    thinking: Option<bool>,
}

/// What the server is asked to hold a reply to, and the probabilities it is asked for, for an [`Output`].
pub(crate) fn constraint_of(output: &Output) -> (Option<Constraint>, Option<u32>) {
    match output {
        Output::Text => (None, None),
        Output::JsonSchema(schema) => (Some(Constraint::JsonSchema(schema.clone())), None),
        Output::Grammar(grammar) => (Some(Constraint::Grammar(grammar.clone())), None),
        // A decision: one word of a closed set, and the probabilities of the tokens the model considered (the server gives at most 20).
        Output::Choice(labels) => (Some(Constraint::Grammar(server::choice_grammar(labels))), Some(20)),
    }
}

fn plan_of(req: &ChatRequest<'_>) -> Plan {
    let (constraint, logprobs) = constraint_of(&req.output);
    // The body fields that sampling and thinking set: empty when nothing is set, and the body is then the one the engine has always sent (temperature 0.3, top_p 0.9).
    let (thinking, fields) = super::sampling::body_parts(&req.sampling, req.reasoning);
    Plan { prompt: req.flattened(), constraint, logprobs, fields, thinking }
}

/// The JSON body that goes to the server for this request: what [`LlamaServerProvider::chat`] sends.
pub fn body_for(req: &ChatRequest<'_>) -> Value {
    let plan = plan_of(req);
    server::build_body(&plan.prompt, req.max_tokens, req.cache.slot, plan.constraint.as_ref(), plan.logprobs, &Extras { thinking: plan.thinking, fields: Some(&plan.fields), ..Extras::default() })
}

/// The outcome of a call from the server's reply: its numbers become the usage, running out of tokens becomes `FinishReason::Length`.
pub(crate) fn outcome_of(reply: Reply, model: &str) -> ChatOutcome {
    let usage = Usage {
        input: reply.timings.prompt_n,
        cached: reply.timings.cache_n,
        output: reply.timings.predicted_n,
        reasoning: None,
        prompt_ms: reply.timings.prompt_ms,
        decode_ms: reply.timings.predicted_ms,
        cost_usd: None,
    };
    let finish = if reply.cut_off { FinishReason::Length } else { FinishReason::Stop };
    ChatOutcome { text: reply.text, tool_calls: Vec::new(), usage, finish, model: ModelRef::new(ID, model), choice_probs: reply.first_token_probs }
}

/// The quantisation named in a GGUF file name ("Qwen3-8B-Q4_K_M.gguf" is "Q4_K_M"), when there is one.
pub fn quant_of(filename: &str) -> Option<String> {
    let stem = filename.rsplit_once('.').filter(|(_, ext)| ext.eq_ignore_ascii_case("gguf")).map_or(filename, |(stem, _)| stem);
    stem.split(['-', '.']).find_map(|word| {
        let upper = word.to_ascii_uppercase();
        let digits = upper.trim_start_matches("IQ").trim_start_matches('Q');
        let is_q = (upper.starts_with('Q') || upper.starts_with("IQ")) && digits.chars().next().is_some_and(|c| c.is_ascii_digit());
        let is_float = matches!(upper.as_str(), "F16" | "BF16" | "F32");
        (is_q || is_float).then_some(upper)
    })
}

fn info_of(m: &GgufModelInfo) -> ModelInfo {
    ModelInfo {
        model: ModelRef::new(ID, m.filename.clone()),
        display_name: m.display_name.clone(),
        family: Family::from_filename(&m.filename),
        caps: Capabilities {
            tools: ToolMode::ConstrainedJson,
            structured: StructuredMode::Gbnf,
            reasoning: ReasoningSupport::Toggle,
            vision: false,
            logprobs: true,
            seed: true,
            prompt_cache: PromptCache::LocalKv { slots: server::slots() },
        },
        limits: Limits { context: server::context_tokens() / server::slots(), max_output: MAX_OUTPUT_TOKENS },
        residency: Residency::Local { bytes: m.size_bytes, quant: quant_of(&m.filename) },
    }
}

impl ModelProvider for LlamaServerProvider {
    fn id(&self) -> &str {
        ID
    }

    fn egress(&self) -> Egress {
        Egress::Local
    }

    fn list(&self) -> Vec<ModelInfo> {
        self.model_dir.as_deref().map(|dir| list_gguf_models(dir).iter().map(info_of).collect()).unwrap_or_default()
    }

    fn chat(&self, model: &str, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        if !req.tools.is_empty() {
            return Err(ProviderError::Rejected("the local model gets its tools in the schema its reply is held to (Output::JsonSchema), not as tool definitions".into()));
        }
        let path = self.path_of(model)?;
        let plan = plan_of(req);
        let (deadline, cancel) = (ctl.deadline, ctl.cancel);
        let extras = Extras { thinking: plan.thinking, fields: Some(&plan.fields), cancel: Some(cancel), idle: ctl.idle, first_event: ctl.first_event };
        let mut tick = |secs: u64| ctl.tick(secs);
        let streaming = req.stream;
        let mut on_text = |piece: &str| matches!(sink(ChatEvent::TextDelta(piece.to_string())), Flow::Continue);
        let on_text: Option<&mut dyn FnMut(&str) -> bool> = if streaming { Some(&mut on_text) } else { None };
        let reply = server::complete_inner(&path, &plan.prompt, req.max_tokens, req.cache.slot, plan.constraint.as_ref(), plan.logprobs, deadline, &mut tick, on_text, &extras)?;

        // A call that was not streamed hands its text over in one piece at the end; a streamed one has already done it piece by piece.
        if !streaming && !reply.text.is_empty() {
            sink(ChatEvent::TextDelta(reply.text.clone()));
        }
        let outcome = outcome_of(reply, model);
        sink(ChatEvent::Usage(outcome.usage));
        sink(ChatEvent::Finish(outcome.finish));
        Ok(outcome)
    }

    fn health(&self) -> Health {
        if server::is_running() {
            Health::Ready
        } else {
            Health::Stopped
        }
    }
}
