//! A scripted model: the provider for tests that need a turn to run without a server.
//!
//! It answers from a list, in order, and remembers every request it was given (the system parts, the messages, what the reply had to look like, the sampling, the slot), so a test can
//! say both "the model said this" and "this is what the model was asked". When the script is used up every further call fails with `Unavailable`, which says so.

use super::error::ProviderError;
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, FinishReason, Flow, Message, ModelRef, Output, Purpose, ReasoningMode, Sampling, Usage};
use super::{Capabilities, Egress, Family, Health, Limits, ModelInfo, ModelProvider, PromptCache, ReasoningSupport, Residency, StructuredMode, ToolMode};
use std::collections::VecDeque;
use std::sync::Mutex;

/// One scripted answer.
#[derive(Clone, Debug)]
pub enum Script {
    /// The call produces this text (in `pieces`, which a streamed call hands over one by one and a plain call joins) and finishes.
    Say { pieces: Vec<String>, finish: FinishReason, usage: Usage, choice_probs: Vec<(String, f64)>, fail_after: Option<ProviderError> },
    /// The call fails.
    Fail(ProviderError),
}

impl Script {
    /// A reply that finished.
    pub fn reply(text: &str) -> Script {
        Script::pieces(&[text])
    }

    /// A reply that ran out of tokens in the middle (`FinishReason::Length`).
    pub fn cut_off(text: &str) -> Script {
        Script::Say { pieces: vec![text.to_string()], finish: FinishReason::Length, usage: Usage::default(), choice_probs: Vec::new(), fail_after: None }
    }

    /// A reply that arrives in these pieces.
    pub fn pieces(pieces: &[&str]) -> Script {
        Script::Say { pieces: pieces.iter().map(|p| p.to_string()).collect(), finish: FinishReason::Stop, usage: Usage::default(), choice_probs: Vec::new(), fail_after: None }
    }

    /// A decision: the word the model wrote, and the probabilities of the tokens it considered.
    pub fn choice(word: &str, probs: &[(&str, f64)]) -> Script {
        Script::Say { pieces: vec![word.to_string()], finish: FinishReason::Stop, usage: Usage::default(), choice_probs: probs.iter().map(|(t, p)| (t.to_string(), *p)).collect(), fail_after: None }
    }

    pub fn fail(error: ProviderError) -> Script {
        Script::Fail(error)
    }

    /// The same, and the usage the server would have measured.
    pub fn with_usage(mut self, u: Usage) -> Script {
        if let Script::Say { usage, .. } = &mut self {
            *usage = u;
        }
        self
    }

    /// The pieces arrive and then the call fails (a server that stopped in the middle of an answer).
    pub fn then_fail(mut self, error: ProviderError) -> Script {
        if let Script::Say { fail_after, .. } = &mut self {
            *fail_after = Some(error);
        }
        self
    }
}

/// What the model was asked, copied out of the request.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    pub model: String,
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub output: Output,
    pub sampling: Sampling,
    pub max_tokens: u32,
    pub reasoning: ReasoningMode,
    pub slot: Option<u32>,
    pub purpose: Purpose,
    pub stream: bool,
    pub tools: usize,
    /// The prompt as one text, as the local provider would send it.
    pub flattened: String,
}

pub struct MockProvider {
    id: String,
    egress: Egress,
    script: Mutex<VecDeque<Script>>,
    seen: Mutex<Vec<Seen>>,
    models: Vec<String>,
}

impl MockProvider {
    /// A local provider called `mock` that answers with these, in order.
    pub fn new(script: Vec<Script>) -> Self {
        Self { id: "mock".into(), egress: Egress::Local, script: Mutex::new(script.into()), seen: Mutex::new(Vec::new()), models: vec!["scripted".into()] }
    }

    /// A local provider called `mock` that answers with these texts, in order.
    pub fn saying(texts: &[&str]) -> Self {
        Self::new(texts.iter().map(|t| Script::reply(t)).collect())
    }

    /// The same provider under another name (for a chain of two).
    pub fn named(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }

    pub fn with_egress(mut self, egress: Egress) -> Self {
        self.egress = egress;
        self
    }

    /// Every request it was given, in order.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How many scripted answers are left.
    pub fn left(&self) -> usize {
        self.script.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl ModelProvider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn egress(&self) -> Egress {
        self.egress.clone()
    }

    fn list(&self) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|m| ModelInfo {
                model: ModelRef::new(self.id.clone(), m.clone()),
                display_name: m.clone(),
                family: Family::Other("mock".into()),
                caps: Capabilities { tools: ToolMode::ConstrainedJson, structured: StructuredMode::Gbnf, reasoning: ReasoningSupport::Toggle, vision: false, logprobs: true, seed: true, prompt_cache: PromptCache::None },
                limits: Limits { context: 4096, max_output: 1200 },
                residency: Residency::Local { bytes: 0, quant: None },
            })
            .collect()
    }

    fn chat(&self, model: &str, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).push(Seen {
            model: model.to_string(),
            system: req.system.iter().map(|s| s.to_string()).collect(),
            messages: req.messages.to_vec(),
            output: req.output.clone(),
            sampling: req.sampling.clone(),
            max_tokens: req.max_tokens,
            reasoning: req.reasoning,
            slot: req.cache.slot,
            purpose: req.purpose,
            stream: req.stream,
            tools: req.tools.len(),
            flattened: req.flattened(),
        });
        let next = self.script.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        let Some(script) = next else {
            return Err(ProviderError::Unavailable { detail: "the script ran out of replies".into() });
        };
        if ctl.cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let (pieces, finish, usage, choice_probs, fail_after) = match script {
            Script::Fail(e) => return Err(e),
            Script::Say { pieces, finish, usage, choice_probs, fail_after } => (pieces, finish, usage, choice_probs, fail_after),
        };
        // A streamed call hands the pieces over one by one; a plain one hands over the whole text once.
        let to_send: Vec<String> = if req.stream { pieces.iter().filter(|p| !p.is_empty()).cloned().collect() } else { vec![pieces.concat()].into_iter().filter(|t| !t.is_empty()).collect() };
        for piece in to_send {
            let flow = sink(ChatEvent::TextDelta(piece));
            // Hanging up is for a stream: the text of a plain call is already complete when it is handed over, as with the local provider.
            if req.stream && (flow == Flow::Stop || ctl.cancelled()) {
                return Err(ProviderError::Cancelled);
            }
        }
        if let Some(e) = fail_after {
            return Err(e);
        }
        sink(ChatEvent::Usage(usage));
        sink(ChatEvent::Finish(finish));
        Ok(ChatOutcome { text: pieces.concat(), tool_calls: Vec::new(), usage, finish, model: ModelRef::new(self.id.clone(), model), choice_probs })
    }

    fn health(&self) -> Health {
        Health::Ready
    }
}
