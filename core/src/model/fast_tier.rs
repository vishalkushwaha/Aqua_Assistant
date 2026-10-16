//! A small second model for the cheap roles, and the rule that falls back to the main model (card OA9-06, task PR-11 of the OpenCode audit).
//!
//! **What is built.** The routing decision and its fall back, against the model seam (`ModelProvider`, `ModelRouter`, the typed errors of card OA9-02), tested with scripted providers:
//!
//! * `AQUA_FAST_TIER` (off) with `AQUA_FAST_TIER_MODEL` (a `.gguf` file name in the model folder: the owner's file, never downloaded) names a smaller model for the roles **decision, fast and
//!   diagnosis** ([`TIERED_ROLES`]). [`Plan`] says which model answers which role and why a fast tier is not in use (the switch is off, no file is named, the file is not in the folder or
//!   is the main model). With the switch off every role is answered by the main model, exactly as before; no other role is ever moved.
//! * [`FastTier::call`] asks the fast model first, for at most `AQUA_FAST_TIER_SECS` (default 20) of the call's own time, and asks the **main model** when the fast one is missing or cannot be
//!   started ([`ProviderError::Unavailable`]), is too slow ([`ProviderError::Timeout`]), is busy ([`ProviderError::Overloaded`] or [`ProviderError::RateLimited`]) or has a window the prompt does
//!   not fit ([`ProviderError::ContextOverflow`]). A refusal, the person's Stop and an unreadable answer are not reasons to ask another model, and an answer that has begun is never asked
//!   again. After a fast model that is missing or too slow, the next calls go straight to the main model for five minutes, so that a model that cannot start is not asked at every call.
//!
//! **The second model server, and the wiring (second round of the card).** The one launch (`llama_server.rs`) holds one process in one static, and a call that names another model replaces
//! it: the first model is unloaded and the second loaded (10 to 40 s measured with the 8B) and both prompt caches are lost. So the small model has a process of its own
//! (`modules::llm_router::fast_server`: lazy, the processor only, one slot, a small window, stopped when idle and with Aqua, left alone for five minutes after a failure), and
//! `model::fast_route` is the one caller of [`FastTier::call`]: the bridge asks it for the roles decision and fast, and with the switch off it says none. **Never run on a model**; what the
//! experiment `fast_tier` of `scripts/eval/speed_experiments/run_experiment.py` measures is whether a small model is **accurate enough** to be worth having (PR-11 step a: the bake-off of
//! the candidate against the 8B), not the routing. **Not routed:** the role diagnosis, which nothing in the engine asks for yet.

use super::error::ProviderError;
use super::roles;
use super::router::ModelRouter;
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, Flow, ModelRef, Role};
use crate::flags;
use serde_json::{json, Value};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The roles a fast model may answer. Everything else (the agent's planner, chat, edits, summaries) is the main model's, whatever the switch says.
pub const TIERED_ROLES: [Role; 3] = [Role::Decision, Role::Fast, Role::Diagnosis];

/// How long the main model is used alone after the fast model was missing or too slow.
pub const COOLDOWN: Duration = Duration::from_secs(300);

/// Is this role answered by the fast tier, when there is one?
pub fn is_tiered(role: Role) -> bool {
    TIERED_ROLES.contains(&role)
}

/// What the person asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub on: bool,
    /// `AQUA_FAST_TIER_MODEL`: a file name of the model folder.
    pub model: Option<String>,
    /// `AQUA_FAST_TIER_SECS`: how long the fast model may take before the main model is asked.
    pub secs: u64,
}

impl Config {
    /// From the environment (and a test's pinned values).
    pub fn from_env() -> Config {
        Config { on: flags::is_on(&flags::FAST_TIER), model: std::env::var("AQUA_FAST_TIER_MODEL").ok(), secs: flags::int(&flags::FAST_TIER_SECS).max(1) as u64 }
    }
}

/// Which model answers which role: the main model for all of them, and a fast one for the tiered roles when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Did the person switch the fast tier on?
    pub on: bool,
    pub main: ModelRef,
    pub fast: Option<ModelRef>,
    /// Why there is no fast tier, in a sentence for the person; empty when there is one.
    pub reason: String,
}

impl Plan {
    /// The plan for this config, the files in the model folder and the main model's file. `provider` names the provider of the models (the local server's name in the program).
    pub fn new(provider: &str, config: &Config, installed: &[String], main: &str) -> Plan {
        let main_ref = ModelRef::new(provider, main);
        let none = |why: String| Plan { on: config.on, main: main_ref.clone(), fast: None, reason: why };
        if !config.on {
            return none("AQUA_FAST_TIER is off".to_string());
        }
        let Some(name) = config.model.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
            return none("AQUA_FAST_TIER_MODEL does not name a file: name the small model's .gguf file in the model folder (Aqua never downloads one)".to_string());
        };
        if !roles::is_plain_file_name(name) {
            return none("AQUA_FAST_TIER_MODEL is not the name of a .gguf file of the model folder (no folder, no path)".to_string());
        }
        let Some(found) = installed.iter().find(|f| f.eq_ignore_ascii_case(name)) else {
            return none("the file named by AQUA_FAST_TIER_MODEL is not in the model folder, so the main model answers every role".to_string());
        };
        if found.eq_ignore_ascii_case(main) {
            return none("the fast tier's file is the main model itself, so there is nothing to route".to_string());
        }
        Plan { on: true, main: main_ref, fast: Some(ModelRef::new(provider, found.clone())), reason: String::new() }
    }

    /// The models that may answer `role`, first to last: the fast one and then the main one for a tiered role of a plan that has a fast tier, else the main one alone.
    pub fn chain(&self, role: Role) -> Vec<ModelRef> {
        match (&self.fast, is_tiered(role)) {
            (Some(fast), true) => vec![fast.clone(), self.main.clone()],
            _ => vec![self.main.clone()],
        }
    }
}

/// What a call came to, counted since the program started (and shown by the status).
static ANSWERED_BY_FAST: AtomicU64 = AtomicU64::new(0);

/// How many calls the fast model has answered since the program started. The engine reads it before and after a decision, to know whether the main model's slot was touched.
pub fn answered_by_fast() -> u64 {
    ANSWERED_BY_FAST.load(Ordering::Relaxed)
}
static ESCALATED: AtomicU64 = AtomicU64::new(0);
static ANSWERED_BY_MAIN: AtomicU64 = AtomicU64::new(0);

/// A fast tier that asks the fast model first and falls back to the main one.
pub struct FastTier {
    plan: Plan,
    secs: Duration,
    cooldown: Duration,
    now: Box<dyn Fn() -> Instant + Send + Sync>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    down_until: Option<Instant>,
    last: Option<String>,
}

/// Is this event a piece of the answer (text, thinking, a tool call), as against the numbers and the end that come after it?
fn is_output(event: &ChatEvent) -> bool {
    match event {
        ChatEvent::TextDelta(text) | ChatEvent::ReasoningDelta(text) => !text.is_empty(),
        ChatEvent::ToolCallStart { .. } | ChatEvent::ToolCallDelta { .. } | ChatEvent::ToolCall { .. } => true,
        ChatEvent::Usage(_) | ChatEvent::Finish(_) => false,
    }
}

/// Is this a reason to ask the main model? A fast model that is missing, too slow, busy, or has too small a window. A refusal of the request, the person's Stop, a call given up for a
/// person who waits and an answer that cannot be read are not: the main model would be refused or stopped the same way.
pub fn escalates(error: &ProviderError) -> bool {
    matches!(error, ProviderError::Unavailable { .. } | ProviderError::Timeout { .. } | ProviderError::Overloaded { .. } | ProviderError::RateLimited { .. } | ProviderError::ContextOverflow { .. })
}

/// Is this a reason to leave the fast model alone for a while? One that is missing or too slow.
fn keeps_away(error: &ProviderError) -> bool {
    matches!(error, ProviderError::Unavailable { .. } | ProviderError::Timeout { .. })
}

impl FastTier {
    pub fn new(plan: Plan, secs: u64) -> FastTier {
        FastTier { plan, secs: Duration::from_secs(secs.max(1)), cooldown: COOLDOWN, now: Box::new(Instant::now), state: Mutex::new(State::default()) }
    }

    /// A tier with its own clock and cooldown (tests).
    pub fn with_clock(mut self, cooldown: Duration, now: Box<dyn Fn() -> Instant + Send + Sync>) -> FastTier {
        self.cooldown = cooldown;
        self.now = now;
        self
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    fn cooling_down(&self) -> Option<Duration> {
        let until = self.state.lock().unwrap_or_else(|e| e.into_inner()).down_until?;
        let now = (self.now)();
        (until > now).then(|| until - now)
    }

    /// A call for `role`. A role that is not tiered, a plan with no fast tier and a fast tier in its cooldown go to the main model alone, as a call on it. **The one caller is the
    /// bridge (`bridge.rs`), through `fast_route::tier_for`** (see the header of this file).
    pub fn call(&self, router: &ModelRouter, role: Role, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        let chain = self.plan.chain(role);
        let (Some(fast), true) = (chain.first().filter(|_| chain.len() == 2), self.cooling_down().is_none()) else {
            let result = router.call_on(&self.plan.main, role, req, ctl, sink);
            if result.is_ok() {
                ANSWERED_BY_MAIN.fetch_add(1, Ordering::Relaxed);
            }
            return result;
        };
        let whole = ctl.deadline;
        let began = (self.now)();
        let spoke = Cell::new(false);
        let mut watching = |event: ChatEvent| {
            if is_output(&event) {
                spoke.set(true);
            }
            sink(event)
        };
        ctl.deadline = whole.min(self.secs);
        let first = router.call_on(fast, role, req, ctl, &mut watching);
        ctl.deadline = whole;
        let error = match first {
            Ok(outcome) => {
                ANSWERED_BY_FAST.fetch_add(1, Ordering::Relaxed);
                return Ok(outcome);
            }
            Err(e) if !spoke.get() && escalates(&e) => e,
            Err(e) => return Err(e),
        };
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.last = Some(error.class().to_string());
            if keeps_away(&error) {
                state.down_until = Some((self.now)() + self.cooldown);
            }
        }
        ESCALATED.fetch_add(1, Ordering::Relaxed);
        // The main model has what is left of the call's own time, not a whole new one.
        ctl.deadline = whole.saturating_sub((self.now)().saturating_duration_since(began));
        let result = router.call_on(&self.plan.main, role, req, ctl, sink);
        ctl.deadline = whole;
        if result.is_ok() {
            ANSWERED_BY_MAIN.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// What this tier is and has done, for the status.
    pub fn report(&self) -> Value {
        let cooling = self.cooling_down();
        let mut v = report_of(&self.plan, self.secs.as_secs());
        v["cooling_down_for_seconds"] = json!(cooling.map(|d| d.as_secs()));
        v["last_reason_for_asking_the_main_model"] = json!(self.state.lock().unwrap_or_else(|e| e.into_inner()).last.clone());
        v
    }
}

fn report_of(plan: &Plan, secs: u64) -> Value {
    json!({
        "switch": if plan.on { "on" } else { "off" },
        "in_use": plan.fast.is_some(),
        "fast_model": plan.fast.as_ref().map(|m| m.model.clone()),
        "main_model": plan.main.model,
        "why_not": (!plan.reason.is_empty()).then(|| plan.reason.clone()),
        "roles": TIERED_ROLES.iter().map(|r| r.name()).collect::<Vec<_>>(),
        "seconds_before_the_main_model_is_asked": secs,
        "answered_by_the_fast_model": ANSWERED_BY_FAST.load(Ordering::Relaxed),
        "asked_the_main_model_after_the_fast_one_failed": ESCALATED.load(Ordering::Relaxed),
        "answered_by_the_main_model": ANSWERED_BY_MAIN.load(Ordering::Relaxed),
        "built": {
            "the_decision_and_the_fall_back": true,
            "the_launch_of_a_second_server": true,
            "called_by_the_engine": true,
            "roles_called_by_the_engine": ["decision", "fast"],
            "not_built": "the role diagnosis (nothing in the engine asks for it), a typed field in GET /api/v1/status, and any measurement: the second server was tried only against a fake model server, and whether the small model is accurate enough and faster is what the experiment fast_tier says",
        },
    })
}

/// The status of the fast tier for the person: from the environment, the files of the model folder and the main model.
pub fn report_now(installed: &[String], main: &str) -> Value {
    let config = Config::from_env();
    report_of(&Plan::new(roles::LOCAL_PROVIDER, &config, installed, main), config.secs)
}
