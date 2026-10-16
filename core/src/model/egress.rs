//! The one gate a model call passes before it is made (card OA9-04, task PR-08 of the OpenCode audit).
//!
//! Until this file the router had one rule written into its loop: a provider on another machine is not asked. The rule is now a thing with a name, so that the place that decides is one
//! place and what it decided is kept: [`EgressGate`] says yes or no to a call before the router makes it ([`EgressGate::authorize`]) and is told how the call ended
//! ([`EgressGate::settle`]). The router has exactly one, and every provider, whatever it is, is asked of it.
//!
//! **What ships: [`LocalOnlyGate`].**
//!
//! | The provider is | The gate says |
//! | :--- | :--- |
//! | `Local` (the server Aqua starts) | yes. Nothing leaves the machine |
//! | `Loopback` (an Ollama or an LM Studio on 127.0.0.1) | yes, with no question to the person. The text stays on the machine (owner decision D-17, the recommended default); it is written in the log of the gate |
//! | `Remote` (another machine) | **no**, with the reason in a sentence. Not by a switch: there is no way to say yes |
//!
//! The reason a remote model has no way to get a yes is that the four rules it needs are not all built: the cleaning of secrets and personal data from the text, a yes from the person in
//! the app for each call (never from the API token or a voice), a cap on what is spent, and a ledger. They exist in the owner's `core/src/remote_model/`, which is not in the
//! repository, so this gate is built to take it in and does not copy it: a remote gate is another implementation of [`EgressGate`] (its `authorize` is where the cleaning, the question
//! and the cap are, its `settle` is where the ledger line is written), given to the router with `ModelRouter::with_gate`. Nothing else in the model layer changes when it comes.
//!
//! **The log.** [`LocalOnlyGate`] keeps the last 100 decisions, in memory, with no text of a prompt or an answer and no key: which provider and model, which role, how big the prompt was,
//! whether the call was allowed and how it ended, and the tokens the service measured. `GET /api/v1/models/providers` shows it.

use super::types::{Purpose, Role, Usage};
use super::Egress;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where the answer is made, as a word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Local,
    Loopback,
    Remote,
}

impl Class {
    pub fn of(egress: &Egress) -> Class {
        match egress {
            Egress::Local => Class::Local,
            Egress::Loopback => Class::Loopback,
            Egress::Remote { .. } => Class::Remote,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Class::Local => "local",
            Class::Loopback => "loopback",
            Class::Remote => "remote",
        }
    }
}

/// What the router is about to do.
#[derive(Clone, Debug)]
pub struct EgressRequest<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub egress: &'a Egress,
    pub role: Role,
    pub purpose: Purpose,
    /// How many characters of prompt the call carries (what would leave, if it left).
    pub prompt_chars: usize,
}

/// A yes. It names the call so that the end of the call can be written against it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressGrant {
    pub id: u64,
    pub class: Class,
    pub provider: String,
    pub model: String,
}

/// A no, in a sentence the person can act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressDenied {
    pub reason: String,
}

/// How a call ended, for the gate's ledger.
#[derive(Clone, Debug)]
pub struct Settled {
    pub ok: bool,
    /// The class of the error (`ProviderError::class`), when there was one.
    pub error: Option<&'static str>,
    pub usage: Option<Usage>,
}

/// Decides whether a model call may be made, and keeps the record of it.
pub trait EgressGate: Send + Sync {
    fn authorize(&self, request: &EgressRequest<'_>) -> Result<EgressGrant, EgressDenied>;

    /// The call that was granted has ended. Called once per grant, whatever the end was.
    fn settle(&self, grant: &EgressGrant, done: &Settled);
}

/// One decision of the gate, as kept in its log. No text of a prompt or an answer, and no key.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub id: u64,
    pub at_ms: u64,
    pub provider: String,
    pub model: String,
    pub role: String,
    pub class: Class,
    pub allowed: bool,
    /// Why not, for a no.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub prompt_chars: usize,
    /// How the call ended: `None` until it has (and for a no).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// What the gate has done so far.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GateReport {
    pub allowed: u64,
    pub refused: u64,
    /// The newest last.
    pub recent: Vec<Decision>,
}

const LOG_SIZE: usize = 100;

#[derive(Default)]
struct Inner {
    next_id: u64,
    allowed: u64,
    refused: u64,
    log: VecDeque<Decision>,
}

/// The gate that ships: what is on this machine may be asked, and anything else may not.
#[derive(Default)]
pub struct LocalOnlyGate {
    inner: Mutex<Inner>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl LocalOnlyGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The counts and the last decisions.
    pub fn report(&self) -> GateReport {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        GateReport { allowed: inner.allowed, refused: inner.refused, recent: inner.log.iter().cloned().collect() }
    }

    fn push(inner: &mut Inner, decision: Decision) {
        if inner.log.len() >= LOG_SIZE {
            inner.log.pop_front();
        }
        inner.log.push_back(decision);
    }
}

/// What the person is told when a model on another machine is asked for.
pub fn remote_refusal(provider: &str, model: &str, host: &str) -> String {
    format!("{provider}/{model} is on another machine ({host}), and a call there needs the person's yes, which is not built yet: the gate that cleans the text, asks for each call, caps what is spent and keeps a ledger is not in this build, so nothing is sent")
}

impl EgressGate for LocalOnlyGate {
    fn authorize(&self, request: &EgressRequest<'_>) -> Result<EgressGrant, EgressDenied> {
        let class = Class::of(request.egress);
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.next_id += 1;
        let id = inner.next_id;
        let mut decision = Decision {
            id,
            at_ms: now_ms(),
            provider: request.provider.to_string(),
            model: request.model.to_string(),
            role: request.role.name().to_string(),
            class,
            allowed: class != Class::Remote,
            reason: None,
            prompt_chars: request.prompt_chars,
            ok: None,
            error: None,
            tokens_in: 0,
            tokens_out: 0,
        };
        match request.egress {
            Egress::Local | Egress::Loopback => {
                inner.allowed += 1;
                Self::push(&mut inner, decision);
                Ok(EgressGrant { id, class, provider: request.provider.to_string(), model: request.model.to_string() })
            }
            Egress::Remote { host } => {
                let reason = remote_refusal(request.provider, request.model, host);
                inner.refused += 1;
                decision.reason = Some(reason.clone());
                Self::push(&mut inner, decision);
                Err(EgressDenied { reason })
            }
        }
    }

    fn settle(&self, grant: &EgressGrant, done: &Settled) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = inner.log.iter_mut().rev().find(|d| d.id == grant.id) {
            entry.ok = Some(done.ok);
            entry.error = done.error.map(str::to_string);
            if let Some(usage) = done.usage {
                entry.tokens_in = usage.input + usage.cached;
                entry.tokens_out = usage.output;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{CallControl, ChatRequest, Flow, Message, ModelRef};
    use crate::model::{MockProvider, ModelProvider, ModelRouter, ProviderError, Script};
    use std::sync::Arc;
    use std::time::Duration;

    fn request(egress: &Egress) -> EgressRequest<'_> {
        EgressRequest { provider: "ollama", model: "qwen3:8b", egress, role: Role::Fast, purpose: Purpose::Chat, prompt_chars: 120 }
    }

    fn ask(router: &ModelRouter, model: &ModelRef) -> Result<crate::model::ChatOutcome, ProviderError> {
        let messages = [Message::user("a secret prompt of a synthetic workspace")];
        let mut ctl = CallControl::with_deadline(Duration::from_secs(5));
        router.call_on(model, Role::Fast, &ChatRequest::new(&messages, 8, Purpose::Chat), &mut ctl, &mut |_| Flow::Continue)
    }

    #[test]
    fn what_is_on_this_machine_is_let_through_with_no_question_and_is_written_in_the_log_without_text() {
        let gate = LocalOnlyGate::new();
        for egress in [Egress::Local, Egress::Loopback] {
            let grant = gate.authorize(&request(&egress)).expect("a yes");
            assert_eq!((grant.class, grant.provider.as_str(), grant.model.as_str()), (Class::of(&egress), "ollama", "qwen3:8b"));
        }
        let report = gate.report();
        assert_eq!((report.allowed, report.refused, report.recent.len()), (2, 0, 2));
        assert_eq!((report.recent[0].class, report.recent[1].class), (Class::Local, Class::Loopback));
        assert!(report.recent.iter().all(|d| d.allowed && d.reason.is_none() && d.prompt_chars == 120 && d.ok.is_none()), "{report:?}");
        let text = serde_json::to_string(&report).unwrap();
        assert!(!text.contains("secret") && !text.contains("prompt\""), "the log holds sizes and names and no text: {text}");
    }

    #[test]
    fn another_machine_is_refused_with_the_reason_and_the_refusal_is_in_the_log() {
        let gate = LocalOnlyGate::new();
        let remote = Egress::Remote { host: "api.example.com".into() };
        let denied = gate.authorize(&request(&remote)).unwrap_err();
        assert!(denied.reason.contains("ollama/qwen3:8b") && denied.reason.contains("api.example.com") && denied.reason.contains("person's yes") && denied.reason.contains("nothing is sent"), "{}", denied.reason);
        let report = gate.report();
        assert_eq!((report.allowed, report.refused), (0, 1));
        assert!(!report.recent[0].allowed && report.recent[0].class == Class::Remote && report.recent[0].reason.as_deref() == Some(denied.reason.as_str()));
    }

    #[test]
    fn the_end_of_a_call_is_written_against_its_grant_and_the_log_keeps_only_the_last_hundred() {
        let gate = LocalOnlyGate::new();
        let egress = Egress::Loopback;
        let first = gate.authorize(&request(&egress)).unwrap();
        let second = gate.authorize(&request(&egress)).unwrap();
        gate.settle(&second, &Settled { ok: false, error: Some("timeout"), usage: None });
        gate.settle(&first, &Settled { ok: true, error: None, usage: Some(crate::model::types::Usage { input: 7, cached: 20, output: 4, ..Default::default() }) });
        let report = gate.report();
        assert_eq!((report.recent[0].ok, report.recent[0].tokens_in, report.recent[0].tokens_out), (Some(true), 27, 4));
        assert_eq!((report.recent[1].ok, report.recent[1].error.as_deref()), (Some(false), Some("timeout")));
        for _ in 0..150 {
            gate.authorize(&request(&egress)).unwrap();
        }
        let report = gate.report();
        assert_eq!((report.recent.len(), report.allowed), (100, 152), "the counts are for ever, the list is the last hundred");
        // A grant whose line is gone is settled without a fuss.
        gate.settle(&first, &Settled { ok: true, error: None, usage: None });
    }

    #[test]
    fn the_router_asks_its_gate_before_every_candidate_and_tells_it_how_the_call_ended() {
        let gate = Arc::new(LocalOnlyGate::new());
        let local = Arc::new(MockProvider::new(vec![Script::reply("fine").with_usage(crate::model::types::Usage { input: 5, output: 2, ..Default::default() })]).named("lan"));
        let remote = Arc::new(MockProvider::saying(&["leaked"]).named("far").with_egress(Egress::Remote { host: "api.example.com".into() }));
        let router = ModelRouter::new(vec![local.clone() as Arc<dyn ModelProvider>, remote.clone()]).with_gate(gate.clone());
        // A role whose first model is on another machine and whose second is here: the first is refused by the gate, the second answers.
        let mut roles = crate::model::RoleTable::new();
        roles.set(Role::Fast, vec![ModelRef::new("far", "big"), ModelRef::new("lan", "small")]);
        let router = router.with_roles(roles);
        let messages = [Message::user("a secret prompt")];
        let mut ctl = CallControl::with_deadline(Duration::from_secs(5));
        let out = router.call(Role::Fast, &ChatRequest::new(&messages, 8, Purpose::Chat), &mut ctl, &mut |_| Flow::Continue).expect("the second model answers");
        assert_eq!(out.text, "fine");
        assert!(remote.seen().is_empty(), "nothing was sent to the other machine");
        let report = gate.report();
        assert_eq!((report.allowed, report.refused), (1, 1));
        assert_eq!((report.recent[0].provider.as_str(), report.recent[0].allowed), ("far", false));
        assert_eq!((report.recent[1].provider.as_str(), report.recent[1].ok, report.recent[1].tokens_in, report.recent[1].tokens_out), ("lan", Some(true), 5, 2));
    }

    #[test]
    fn a_refusal_is_counted_by_the_function_the_router_was_given_and_a_yes_is_not() {
        fn note(name: &str, labels: &[(&str, &str)]) {
            NOTED.with(|n| n.borrow_mut().push(format!("{name}{labels:?}")));
        }
        thread_local! {
            static NOTED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let remote = Arc::new(MockProvider::saying(&["leaked"]).named("far").with_egress(Egress::Remote { host: "api.example.com".into() }));
        let here = Arc::new(MockProvider::saying(&["fine"]).named("lan"));
        let router = ModelRouter::new(vec![remote as Arc<dyn ModelProvider>, here]).with_counters(note);
        assert!(ask(&router, &ModelRef::new("far", "big")).is_err());
        assert!(ask(&router, &ModelRef::new("lan", "small")).is_ok());
        let noted = NOTED.with(|n| n.borrow().clone());
        assert_eq!(noted, vec![r#"aqua_model_egress_refused_total[("class", "remote")]"#.to_string()]);
    }

    /// The gate is where a remote model would be let in: a gate that says yes to another machine, and nothing else changed in the router.
    struct AllowEverything(Mutex<Vec<String>>);

    impl EgressGate for AllowEverything {
        fn authorize(&self, request: &EgressRequest<'_>) -> Result<EgressGrant, EgressDenied> {
            self.0.lock().unwrap().push(format!("authorize {}/{} {}", request.provider, request.model, Class::of(request.egress).word()));
            Ok(EgressGrant { id: 1, class: Class::of(request.egress), provider: request.provider.to_string(), model: request.model.to_string() })
        }

        fn settle(&self, _: &EgressGrant, done: &Settled) {
            self.0.lock().unwrap().push(format!("settle ok={}", done.ok));
        }
    }

    #[test]
    fn a_gate_that_says_yes_to_another_machine_is_all_a_remote_model_needs_to_be_asked() {
        let remote = Arc::new(MockProvider::saying(&["from far away"]).named("far").with_egress(Egress::Remote { host: "api.example.com".into() }));
        let gate = Arc::new(AllowEverything(Mutex::new(Vec::new())));
        let router = ModelRouter::new(vec![remote.clone() as Arc<dyn ModelProvider>]).with_gate(gate.clone());
        assert_eq!(ask(&router, &ModelRef::new("far", "big")).unwrap().text, "from far away");
        assert_eq!(remote.seen().len(), 1);
        assert_eq!(*gate.0.lock().unwrap(), ["authorize far/big remote", "settle ok=true"]);
        // And the router that has no such gate does not ask it.
        let plain = ModelRouter::new(vec![remote.clone() as Arc<dyn ModelProvider>]);
        assert!(ask(&plain, &ModelRef::new("far", "big")).is_err());
        assert_eq!(remote.seen().len(), 1, "still the one");
    }
}
