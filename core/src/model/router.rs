//! Which model answers which role, and the call that finds the provider and runs it.
//!
//! The router is small on purpose. It looks a provider up by the name in a [`ModelRef`], refuses one that is not on this machine, runs the call, and when a candidate of the role's
//! chain cannot be used (`ProviderError::Unavailable`, or a service that said "later" and went on saying it) tries the next one. Every other error comes back to the caller untouched:
//! a refusal, a Stop, a yield are not reasons to try another model. The per-model quirks are the next card.
//!
//! **Who may be asked** is not decided here but by the router's [`EgressGate`] (card OA9-04, `egress.rs`): it is asked before each candidate, and a "no" is an `Unavailable` that says why
//! and lets the next candidate be tried, exactly as a model that is not on this machine always was. The gate that ships says yes to what is on this machine (the local server, a
//! loopback provider) and no to anything else.
//!
//! What it does for one candidate (card OA9-02, task PR-03), in this order, and never once a piece of the answer has been handed to the caller (a second try would say it again, so the
//! error is final):
//!
//! * a service that says "later" ([`ProviderError::RateLimited`], [`ProviderError::Overloaded`]) is asked again after the wait it asked for, or a doubling one, up to the router's
//!   [`RetryPolicy`] and never past the call's own time limit (`retry.rs`). The local server never says it, so for the local model nothing changes;
//! * a prompt the provider refused as too long ([`ProviderError::ContextOverflow`]) is given to the caller's `CallControl::shrink`, once. If it returns the messages of the same turn
//!   with less in them the call is made again with those; if it returns nothing, or there is no such function, the refusal goes back. A second refusal is final. Nothing here shortens
//!   a prompt by itself: only the caller knows what can be left out (the agent loop does it one level up, with the regions of its prompt, and that is the recovery the planner uses).

use super::egress::{EgressGate, EgressRequest, LocalOnlyGate, Settled};
use super::error::ProviderError;
use super::retry::{self, RetryPolicy};
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, Flow, ModelRef, Role};
use super::ModelProvider;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tracing::{debug, info};

/// For each role, the models that may answer it: the first is tried first, the others only when it cannot be used. A role with no chain has no model until someone assigns one.
#[derive(Clone, Debug, Default)]
pub struct RoleTable {
    chains: HashMap<Role, Vec<ModelRef>>,
}

impl RoleTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every role answered by the same single model.
    pub fn all(model: ModelRef) -> Self {
        let mut t = Self::new();
        for role in Role::ALL {
            t.set(role, vec![model.clone()]);
        }
        t
    }

    pub fn set(&mut self, role: Role, chain: Vec<ModelRef>) {
        if chain.is_empty() {
            self.chains.remove(&role);
        } else {
            self.chains.insert(role, chain);
        }
    }

    pub fn chain(&self, role: Role) -> &[ModelRef] {
        self.chains.get(&role).map(Vec::as_slice).unwrap_or(&[])
    }
}

pub struct ModelRouter {
    providers: Vec<Arc<dyn ModelProvider>>,
    roles: RoleTable,
    retry: RetryPolicy,
    /// Says yes or no to each call before it is made (card OA9-04). Local-only unless this router was given another.
    gate: Arc<dyn EgressGate>,
    /// Where this router writes its counts; none: the one the program installed ([`set_counter_hook`]), and without that, nowhere.
    counters: Option<CounterHook>,
}

/// What the router does with a count: its name (`aqua_model_retries_total`) and its labels. The metrics are kept by a module above the model layer (`metrics`), which the model layer may
/// not name (core/tests/layering_test.rs), so the program gives the router the function that adds to them: `core/src/main.rs` installs `metrics::inc` at start-up with
/// [`set_counter_hook`], and a router can carry its own ([`ModelRouter::with_counters`]).
pub type CounterHook = fn(&str, &[(&str, &str)]);

/// The function every router counts with when it carries none; set once by the program.
static APP_COUNTERS: OnceLock<CounterHook> = OnceLock::new();

/// Gives every router that carries no counter function of its own the one that adds to the metrics (see [`CounterHook`]). Called once, by `main`, before the first model call; `false`
/// when one was set already (the first stays). **Until it is set the router counts nothing**: a retry or a shortened prompt then leaves no mark in the metrics.
pub fn set_counter_hook(hook: CounterHook) -> bool {
    APP_COUNTERS.set(hook).is_ok()
}

/// Is this event a piece of the answer (text, thinking, a tool call), as against the numbers and the end that come after it? Once one has been handed over, a call is not asked again.
fn is_output(event: &ChatEvent) -> bool {
    match event {
        ChatEvent::TextDelta(text) | ChatEvent::ReasoningDelta(text) => !text.is_empty(),
        ChatEvent::ToolCallStart { .. } | ChatEvent::ToolCallDelta { .. } | ChatEvent::ToolCall { .. } => true,
        ChatEvent::Usage(_) | ChatEvent::Finish(_) => false,
    }
}

/// How many characters of prompt a call carries, for the gate's record.
fn prompt_chars(req: &ChatRequest<'_>) -> usize {
    req.system.iter().map(|s| s.len()).sum::<usize>() + req.messages.iter().map(|m| m.content.len()).sum::<usize>()
}

impl ModelRouter {
    pub fn new(providers: Vec<Arc<dyn ModelProvider>>) -> Self {
        Self { providers, roles: RoleTable::new(), retry: RetryPolicy::STANDARD, gate: Arc::new(LocalOnlyGate::new()), counters: None }
    }

    /// The gate this router asks before every call (`LocalOnlyGate` unless this says otherwise). A gate that can say yes to another machine is where the cleaning of the text, the
    /// person's yes and the spend cap would be; none is built yet.
    pub fn with_gate(mut self, gate: Arc<dyn EgressGate>) -> Self {
        self.gate = gate;
        self
    }

    pub fn gate(&self) -> &Arc<dyn EgressGate> {
        &self.gate
    }

    /// The function this router adds its counts with, and not the program's ([`set_counter_hook`]); a test uses it so that no process-wide state changes.
    pub fn with_counters(mut self, hook: CounterHook) -> Self {
        self.counters = Some(hook);
        self
    }

    /// Adds one to the count `name` with `labels`, by the function this router has or the program installed; nothing when there is none.
    fn count(&self, name: &str, labels: &[(&str, &str)]) {
        if let Some(hook) = self.counters.or_else(|| APP_COUNTERS.get().copied()) {
            hook(name, labels);
        }
    }

    pub fn with_roles(mut self, roles: RoleTable) -> Self {
        self.roles = roles;
        self
    }

    /// How a call that failed with "later" is asked again (`RetryPolicy::STANDARD` unless this says otherwise; `RetryPolicy::NONE` never asks again).
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry
    }

    pub fn roles(&self) -> &RoleTable {
        &self.roles
    }

    pub fn provider(&self, id: &str) -> Option<&Arc<dyn ModelProvider>> {
        self.providers.iter().find(|p| p.id() == id)
    }

    /// Every model of every provider.
    pub fn list(&self) -> Vec<super::ModelInfo> {
        self.providers.iter().flat_map(|p| p.list()).collect()
    }

    /// A call for `role`: the models the role table names for it, in order. A role with none fails with `Unavailable`, which says so.
    pub fn call(&self, role: Role, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        let chain = self.roles.chain(role);
        if chain.is_empty() {
            return Err(ProviderError::Unavailable { detail: format!("no model is assigned to the role '{}'", role.name()) });
        }
        self.run(chain, role, req, ctl, sink)
    }

    /// A call on a model the caller has already chosen (the engine decides which GGUF the agent and chat use). The role is for the logs.
    pub fn call_on(&self, model: &ModelRef, role: Role, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        self.run(std::slice::from_ref(model), role, req, ctl, sink)
    }

    fn run(&self, chain: &[ModelRef], role: Role, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        let mut first_error: Option<ProviderError> = None;
        for (index, model) in chain.iter().enumerate() {
            let (attempt, spoke) = match self.provider(&model.provider) {
                None => (Err(ProviderError::Unavailable { detail: format!("no provider is called '{}'", model.provider) }), false),
                Some(provider) => {
                    let egress = provider.egress();
                    let request = EgressRequest { provider: &model.provider, model: &model.model, egress: &egress, role, purpose: req.purpose, prompt_chars: prompt_chars(req) };
                    match self.gate.authorize(&request) {
                        Ok(grant) => {
                            debug!("model call: role={} model={model} purpose={:?}", role.name(), req.purpose);
                            let (result, spoke) = self.ask(provider.as_ref(), model, req, ctl, sink);
                            self.gate.settle(&grant, &Settled { ok: result.is_ok(), error: result.as_ref().err().map(ProviderError::class), usage: result.as_ref().ok().map(|o| o.usage) });
                            (result, spoke)
                        }
                        // Not used, and not a reason to stop: a later model of the chain that the gate lets through may still answer.
                        Err(denied) => {
                            self.count("aqua_model_egress_refused_total", &[("class", super::egress::Class::of(&egress).word())]);
                            (Err(ProviderError::Unavailable { detail: denied.reason }), false)
                        }
                    }
                }
            };
            match attempt {
                Ok(outcome) => return Ok(outcome),
                // Only a provider that cannot be used, or a service that went on saying "later", makes the next candidate worth a try, and not when some of the answer is already with the
                // caller; the first candidate's error is the one reported when none works.
                Err(e) if !spoke && matches!(e, ProviderError::Unavailable { .. } | ProviderError::RateLimited { .. } | ProviderError::Overloaded { .. }) => {
                    if index + 1 < chain.len() {
                        info!("model {model} cannot be used ({e}); trying the next model of the role '{}'", role.name());
                    }
                    first_error.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(first_error.unwrap_or(ProviderError::Unavailable { detail: "no model to ask".into() }))
    }

    /// One candidate: asked, and asked again when the rules in the header of this file say so. Gives the result and whether any of the answer was handed to `sink`.
    fn ask(&self, provider: &dyn ModelProvider, model: &ModelRef, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> (Result<ChatOutcome, ProviderError>, bool) {
        let began = Instant::now();
        let whole = ctl.deadline;
        let spoke = Cell::new(false);
        let (mut retries, mut shrink_asked, mut shrunk) = (0u32, false, None::<Vec<super::types::Message>>);
        let mut tried = false;
        let result = loop {
            // The first try has the call's time as it was given; every later one has what is left of it, so that the tries together keep within it.
            if tried {
                ctl.deadline = whole.saturating_sub(began.elapsed());
            }
            tried = true;
            let rebuilt = shrunk.as_ref().map(|messages| ChatRequest { messages, ..req.clone() });
            let current = rebuilt.as_ref().unwrap_or(req);
            let mut watching = |event: ChatEvent| {
                if is_output(&event) {
                    spoke.set(true);
                }
                sink(event)
            };
            let error = match provider.chat(&model.model, current, ctl, &mut watching) {
                Ok(outcome) => break Ok(outcome),
                Err(e) => e,
            };
            // Never after the first piece of the answer: the caller has it, and a second try would say it again.
            if spoke.get() {
                break Err(error);
            }
            if matches!(error, ProviderError::ContextOverflow { .. }) {
                // The caller's way of making the turn shorter, once. A second refusal is final.
                if !shrink_asked {
                    if let Some(shrink) = ctl.shrink.as_mut() {
                        shrink_asked = true;
                        if let Some(messages) = (**shrink)(&error) {
                            info!("model {model}: the prompt did not fit ({error}); asking again with the shorter turn the caller gave");
                            shrunk = Some(messages);
                            continue;
                        }
                    }
                }
                break Err(error);
            }
            match self.retry.delay(&error, retries) {
                Some(wait) if began.elapsed() + wait < whole => {
                    info!("model {model} said later ({error}); asking again in {} ms (ask {} of at most {})", wait.as_millis(), retries + 1, self.retry.max_retries);
                    self.count("aqua_model_retries_total", &[("class", error.class())]);
                    let stopped = || ctl.cancelled() || crate::modules::browser_agent::turn_cancelled();
                    if !retry::wait(wait, &stopped) {
                        break Err(ProviderError::Cancelled);
                    }
                    retries += 1;
                }
                _ => break Err(error),
            }
        };
        ctl.deadline = whole;
        if shrink_asked {
            self.count("aqua_model_shrinks_total", &[("recovered", if shrunk.is_some() && result.is_ok() { "true" } else { "false" })]);
        }
        (result, spoke.get())
    }
}
