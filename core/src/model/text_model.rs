//! One seam for "a prompt in, text out" (task AQ-10).
//!
//! The flows' `Completer`, the agent's `Planner`, the reader's `ReadModel`, the dictation `Rewriter` and `Normaliser` are five traits for one idea, each with its own error type
//! (`String`, `anyhow`). [`TextModel`] is that idea once, with [`ProviderError`] as the error. The five traits are not removed or changed: what is here converts in both directions, so
//! that new code can speak [`TextModel`] and still be given to the flows, the triage and the reader, and an old model can be given to code that wants a [`TextModel`].
//!
//! * [`RouterText`]: a [`TextModel`] on the model router (a role, a model). Give it to [`AsLegacy`] and it is a `Completer`, a `ReadModel`, a `Rewriter` and a `Normaliser`.
//! * [`CompleterText`], [`PlannerText`], [`ReadModelText`], [`RewriterText`], [`NormaliserText`]: the old traits seen as a [`TextModel`].

use super::bridge::{output_of, timings_of};
use super::error::ProviderError;
use super::router::ModelRouter;
use super::types::{CallControl, Flow, Message, ModelRef, Purpose, Role};
use super::ChatRequest;
use crate::dictation::command::Rewriter;
use crate::dictation::model_tier::Normaliser;
use crate::flows::planner::Completer;
use crate::modules::llm_router::llama_server::{Constraint, Timings};
use crate::modules::planner::{PlanRequest, Planner};
use crate::modules::reader::ReadModel;
use std::sync::Arc;
use std::time::Duration;

/// The error of the text seam: the class of the failure, with the message the caller would have read before.
pub type LlmError = ProviderError;

/// One request for text.
#[derive(Clone, Copy, Debug)]
pub struct TextRequest<'a> {
    pub prompt: &'a str,
    /// The most tokens the reply may take.
    pub max_tokens: u32,
    /// What the reply must look like, when the model can be held to it. A model that cannot ignores it.
    pub constraint: Option<&'a Constraint>,
}

impl<'a> TextRequest<'a> {
    pub fn new(prompt: &'a str, max_tokens: u32) -> Self {
        Self { prompt, max_tokens, constraint: None }
    }

    pub fn with_constraint(mut self, constraint: Option<&'a Constraint>) -> Self {
        self.constraint = constraint;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextReply {
    pub text: String,
    /// What the server measured, when it says.
    pub timings: Option<Timings>,
}

pub trait TextModel: Send + Sync {
    /// The model's name, for the run record and the monitor.
    fn name(&self) -> String;

    /// The prompt must fit in this many tokens.
    fn max_context_tokens(&self) -> u32;

    fn complete(&self, req: &TextRequest<'_>) -> Result<TextReply, LlmError>;
}

/// How many tokens a reply may take for an old caller that does not say (the flows' completer).
const DEFAULT_REPLY_TOKENS: u32 = 512;

/// A model of the router, asked for text.
pub struct RouterText {
    router: Arc<ModelRouter>,
    model: ModelRef,
    role: Role,
    purpose: Purpose,
    deadline: Duration,
}

impl RouterText {
    pub fn new(router: Arc<ModelRouter>, model: ModelRef, role: Role) -> Self {
        let purpose = match role {
            Role::Agent => Purpose::Planner,
            Role::Edit => Purpose::Edit,
            Role::Summary => Purpose::Summary,
            Role::Decision => Purpose::Decision,
            Role::Diagnosis => Purpose::Diagnosis,
            Role::Chat | Role::Fast => Purpose::Chat,
        };
        Self { router, model, role, purpose, deadline: Duration::from_secs(300) }
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

impl TextModel for RouterText {
    fn name(&self) -> String {
        self.model.model.clone()
    }

    fn max_context_tokens(&self) -> u32 {
        self.router.list().iter().find(|i| i.model == self.model).map(|i| i.limits.context).unwrap_or(2048)
    }

    fn complete(&self, req: &TextRequest<'_>) -> Result<TextReply, LlmError> {
        let messages = [Message::user(req.prompt)];
        let chat = ChatRequest::new(&messages, req.max_tokens, self.purpose).with_output(output_of(req.constraint));
        let mut ctl = CallControl::with_deadline(self.deadline);
        let out = self.router.call_on(&self.model, self.role, &chat, &mut ctl, &mut |_| Flow::Continue)?;
        Ok(TextReply { text: out.text, timings: Some(timings_of(&out.usage)) })
    }
}

/// A [`TextModel`] given to the code that still asks for the old traits: the flows' `Completer` (and so the triage, the vote and the comment drafts), the reader's `ReadModel`, and the
/// dictation `Rewriter` and `Normaliser`.
pub struct AsLegacy<M: TextModel>(pub M);

impl<M: TextModel> Completer for AsLegacy<M> {
    fn complete(&self, prompt: &str) -> Result<String, String> {
        self.complete_with_budget(prompt, DEFAULT_REPLY_TOKENS)
    }

    fn complete_with_budget(&self, prompt: &str, max_tokens: u32) -> Result<String, String> {
        self.0.complete(&TextRequest::new(prompt, max_tokens)).map(|r| r.text).map_err(|e| e.to_string())
    }
}

impl<M: TextModel> ReadModel for AsLegacy<M> {
    fn complete(&self, prompt: &str, constraint: Option<Constraint>, max_tokens: u32) -> anyhow::Result<String> {
        // The error is the typed one inside an `anyhow`, so a caller can still find the class (`downcast_ref::<ProviderError>()`).
        Ok(self.0.complete(&TextRequest::new(prompt, max_tokens).with_constraint(constraint.as_ref())).map_err(anyhow::Error::new)?.text)
    }
}

impl<M: TextModel> Rewriter for AsLegacy<M> {
    fn rewrite(&self, prompt: &str, max_tokens: u32) -> Result<String, String> {
        self.0.complete(&TextRequest::new(prompt, max_tokens)).map(|r| r.text).map_err(|e| e.to_string())
    }
}

impl<M: TextModel> Normaliser for AsLegacy<M> {
    fn normalise(&self, prompt: &str, max_tokens: u32) -> Result<String, String> {
        self.0.complete(&TextRequest::new(prompt, max_tokens)).map(|r| r.text).map_err(|e| e.to_string())
    }
}

/// The flows' `Completer` as a [`TextModel`]. (It has no constraint to enforce and does not say its window.)
pub struct CompleterText<'a>(pub &'a dyn Completer);

impl TextModel for CompleterText<'_> {
    fn name(&self) -> String {
        "completer".into()
    }

    fn max_context_tokens(&self) -> u32 {
        2048
    }

    fn complete(&self, req: &TextRequest<'_>) -> Result<TextReply, LlmError> {
        self.0.complete_chat(req)
    }
}

/// The agent's `Planner` as a [`TextModel`].
pub struct PlannerText<'a, P: Planner + Sync>(pub &'a P);

impl<P: Planner + Sync> TextModel for PlannerText<'_, P> {
    fn name(&self) -> String {
        self.0.model_name()
    }

    fn max_context_tokens(&self) -> u32 {
        self.0.max_context_tokens()
    }

    fn complete(&self, req: &TextRequest<'_>) -> Result<TextReply, LlmError> {
        let reply = self.0.plan(&PlanRequest { prompt: req.prompt, max_tokens: req.max_tokens, constraint: req.constraint }, &mut |_| {}).map_err(|e| ProviderError::from_anyhow(&e))?;
        Ok(TextReply { text: reply.text, timings: reply.timings })
    }
}

/// The reader's `ReadModel` as a [`TextModel`].
pub struct ReadModelText<'a>(pub &'a dyn ReadModel);

impl TextModel for ReadModelText<'_> {
    fn name(&self) -> String {
        "reader".into()
    }

    fn max_context_tokens(&self) -> u32 {
        2048
    }

    fn complete(&self, req: &TextRequest<'_>) -> Result<TextReply, LlmError> {
        let text = self.0.complete(req.prompt, req.constraint.cloned(), req.max_tokens).map_err(|e| ProviderError::from_anyhow(&e))?;
        Ok(TextReply { text, timings: None })
    }
}

/// The dictation `Rewriter` as a [`TextModel`].
pub struct RewriterText<'a>(pub &'a (dyn Rewriter + Sync));

impl TextModel for RewriterText<'_> {
    fn name(&self) -> String {
        "rewriter".into()
    }

    fn max_context_tokens(&self) -> u32 {
        4096
    }

    fn complete(&self, req: &TextRequest<'_>) -> Result<TextReply, LlmError> {
        self.0.rewrite(req.prompt, req.max_tokens).map(|text| TextReply { text, timings: None }).map_err(|e| ProviderError::from_text(&e))
    }
}

/// The dictation `Normaliser` as a [`TextModel`].
pub struct NormaliserText<'a>(pub &'a dyn Normaliser);

impl TextModel for NormaliserText<'_> {
    fn name(&self) -> String {
        "normaliser".into()
    }

    fn max_context_tokens(&self) -> u32 {
        4096
    }

    fn complete(&self, req: &TextRequest<'_>) -> Result<TextReply, LlmError> {
        self.0.normalise(req.prompt, req.max_tokens).map(|text| TextReply { text, timings: None }).map_err(|e| ProviderError::from_text(&e))
    }
}
