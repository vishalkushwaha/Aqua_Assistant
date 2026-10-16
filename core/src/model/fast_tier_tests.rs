//! Tests of the fast tier (card OA9-06, task PR-11): which model answers which role, and the fall back to the main model when the fast one is missing, cannot start, is too slow or is
//! full. The models are scripted providers: no model, no server.

use super::error::ProviderError;
use super::fast_tier::*;
use super::retry::RetryPolicy;
use super::router::ModelRouter;
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, FinishReason, Flow, Message, ModelRef, Purpose, Role, Usage};
use super::{Egress, ModelInfo, ModelProvider};
use crate::flags;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROVIDER: &str = "scripted";
const FAST: &str = "Qwen3.5-4B-Q4_K_M.gguf";
const MAIN: &str = "Qwen3-8B-Q4_K_M.gguf";

/// What a scripted model does with one call.
#[derive(Clone, Debug)]
enum Behaviour {
    Say(&'static str),
    Fail(ProviderError),
    /// Hands over some words and then fails: an answer that has begun.
    SayThenFail(&'static str, ProviderError),
}

/// A provider with a model that answers as it is told, and a record of every call it had: the model's name and the time the call was given.
struct Spy {
    behaviours: Mutex<HashMap<String, VecDeque<Behaviour>>>,
    seen: Mutex<Vec<(String, Duration)>>,
}

impl Spy {
    fn new(script: &[(&str, Vec<Behaviour>)]) -> Arc<Spy> {
        Arc::new(Spy { behaviours: Mutex::new(script.iter().map(|(m, b)| (m.to_string(), b.iter().cloned().collect())).collect()), seen: Mutex::new(Vec::new()) })
    }

    fn asked(&self) -> Vec<String> {
        self.seen.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
    }

    fn deadlines(&self) -> Vec<Duration> {
        self.seen.lock().unwrap().iter().map(|(_, d)| *d).collect()
    }
}

struct Handle(Arc<Spy>);

impl ModelProvider for Handle {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn egress(&self) -> Egress {
        Egress::Local
    }

    fn list(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    fn chat(&self, model: &str, _req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        self.0.seen.lock().unwrap().push((model.to_string(), ctl.deadline));
        let next = self.0.behaviours.lock().unwrap().get_mut(model).and_then(VecDeque::pop_front);
        let done = |text: &str| ChatOutcome { text: text.to_string(), tool_calls: Vec::new(), usage: Usage::default(), finish: FinishReason::Stop, model: ModelRef::new(PROVIDER, model), choice_probs: Vec::new() };
        match next {
            Some(Behaviour::Say(text)) => {
                sink(ChatEvent::TextDelta(text.to_string()));
                Ok(done(text))
            }
            Some(Behaviour::Fail(e)) => Err(e),
            Some(Behaviour::SayThenFail(text, e)) => {
                sink(ChatEvent::TextDelta(text.to_string()));
                Err(e)
            }
            None => Err(ProviderError::Unavailable { detail: format!("{model} has nothing scripted") }),
        }
    }
}

fn router(spy: &Arc<Spy>) -> ModelRouter {
    ModelRouter::new(vec![Arc::new(Handle(spy.clone()))]).with_retry(RetryPolicy::NONE)
}

fn config(on: bool, model: Option<&str>) -> Config {
    Config { on, model: model.map(str::to_string), secs: 20 }
}

fn installed() -> Vec<String> {
    vec![MAIN.to_string(), FAST.to_string(), "Granite-4.2-3B.gguf".to_string()]
}

fn tier(clock: Option<Arc<Mutex<Instant>>>) -> FastTier {
    let t = FastTier::new(Plan::new(PROVIDER, &config(true, Some(FAST)), &installed(), MAIN), 20);
    match clock {
        Some(c) => t.with_clock(Duration::from_secs(300), Box::new(move || *c.lock().unwrap())),
        None => t,
    }
}

fn ask(t: &FastTier, r: &ModelRouter, role: Role, secs: u64) -> Result<ChatOutcome, ProviderError> {
    let messages = [Message::user("which?")];
    let req = ChatRequest::new(&messages, 1, Purpose::Decision);
    let mut ctl = CallControl::with_deadline(Duration::from_secs(secs));
    let mut heard = Vec::new();
    let result = t.call(r, role, &req, &mut ctl, &mut |e| {
        heard.push(e);
        Flow::Continue
    });
    assert_eq!(ctl.deadline, Duration::from_secs(secs), "the call's own time is put back whatever happened");
    result
}

fn answered_by(r: Result<ChatOutcome, ProviderError>) -> String {
    r.expect("an answer").model.model
}

#[test]
fn which_model_answers_which_role_and_why_there_is_no_fast_tier() {
    let plan = Plan::new(PROVIDER, &config(true, Some(FAST)), &installed(), MAIN);
    assert_eq!(plan.fast.as_ref().map(|m| m.model.as_str()), Some(FAST));
    assert_eq!(plan.reason, "");
    for role in Role::ALL {
        let chain: Vec<String> = plan.chain(role).iter().map(|m| m.model.clone()).collect();
        if [Role::Decision, Role::Fast, Role::Diagnosis].contains(&role) {
            assert_eq!(chain, [FAST, MAIN], "{role:?}: the fast model first and the main model behind it");
        } else {
            assert_eq!(chain, [MAIN], "{role:?} is never moved");
        }
    }
    // Every way of having no fast tier: the main model answers every role, and the plan says why.
    let cases: [(Config, &str); 7] = [
        (config(false, Some(FAST)), "AQUA_FAST_TIER is off"),
        (config(true, None), "does not name a file"),
        (config(true, Some("  ")), "does not name a file"),
        (config(true, Some("../Qwen3.5-4B-Q4_K_M.gguf")), "not the name of a .gguf file"),
        (config(true, Some("C:\\models\\small.gguf")), "not the name of a .gguf file"),
        (config(true, Some("Missing.gguf")), "is not in the model folder"),
        (config(true, Some("qwen3-8b-q4_k_m.GGUF")), "the main model itself"),
    ];
    for (cfg, why) in cases {
        let plan = Plan::new(PROVIDER, &cfg, &installed(), MAIN);
        assert!(plan.fast.is_none(), "{cfg:?}");
        assert!(plan.reason.contains(why), "{cfg:?}: {}", plan.reason);
        for role in Role::ALL {
            assert_eq!(plan.chain(role).len(), 1, "{role:?}");
            assert_eq!(plan.chain(role)[0].model, MAIN);
        }
    }
    // The file name is matched without regard to case, and the folder's own spelling is used.
    let plan = Plan::new(PROVIDER, &config(true, Some("qwen3.5-4b-q4_k_m.gguf")), &installed(), MAIN);
    assert_eq!(plan.fast.unwrap().model, FAST);
}

#[test]
fn the_switch_and_the_seconds_are_read_with_the_defaults_off_and_twenty() {
    flags::with_overrides(&[("AQUA_FAST_TIER", None), ("AQUA_FAST_TIER_SECS", None)], || {
        let c = Config::from_env();
        assert!(!c.on);
        assert_eq!(c.secs, 20);
    });
    flags::with_overrides(&[("AQUA_FAST_TIER", Some("on")), ("AQUA_FAST_TIER_SECS", Some("45"))], || {
        let c = Config::from_env();
        assert!(c.on);
        assert_eq!(c.secs, 45);
    });
    flags::with_overrides(&[("AQUA_FAST_TIER", Some("yes please")), ("AQUA_FAST_TIER_SECS", Some("0"))], || {
        let c = Config::from_env();
        assert!(!c.on, "an unknown word is off");
        assert_eq!(c.secs, 20, "a number out of range is the default");
    });
}

#[test]
fn a_fast_model_that_answers_is_the_only_model_asked() {
    let spy = Spy::new(&[(FAST, vec![Behaviour::Say("A")]), (MAIN, vec![Behaviour::Say("never")])]);
    let t = tier(None);
    assert_eq!(answered_by(ask(&t, &router(&spy), Role::Decision, 60)), FAST);
    assert_eq!(spy.asked(), [FAST]);
}

#[test]
fn a_role_that_is_not_tiered_and_a_plan_with_no_fast_tier_go_to_the_main_model_alone() {
    let spy = Spy::new(&[(FAST, vec![Behaviour::Say("fast")]), (MAIN, vec![Behaviour::Say("m1"), Behaviour::Say("m2"), Behaviour::Say("m3"), Behaviour::Say("m4"), Behaviour::Say("m5"), Behaviour::Say("m6")])]);
    let t = tier(None);
    for role in [Role::Agent, Role::Chat, Role::Edit, Role::Summary] {
        assert_eq!(answered_by(ask(&t, &router(&spy), role, 60)), MAIN, "{role:?}");
    }
    let off = FastTier::new(Plan::new(PROVIDER, &config(false, Some(FAST)), &installed(), MAIN), 20);
    for role in [Role::Decision, Role::Fast] {
        assert_eq!(answered_by(ask(&off, &router(&spy), role, 60)), MAIN, "{role:?}: with the switch off no model answers anything differently");
    }
    assert!(!spy.asked().contains(&FAST.to_string()), "the fast model was never asked: {:?}", spy.asked());
}

#[test]
fn a_fast_model_that_is_missing_or_cannot_start_is_replaced_by_the_main_model_at_once_and_left_alone_for_a_while() {
    let spy = Spy::new(&[
        (FAST, vec![Behaviour::Fail(ProviderError::Unavailable { detail: "llama-server exited while loading the model".into() }), Behaviour::Say("fast again")]),
        (MAIN, vec![Behaviour::Say("m1"), Behaviour::Say("m2"), Behaviour::Say("m3"), Behaviour::Say("m4")]),
    ]);
    let clock = Arc::new(Mutex::new(Instant::now()));
    let t = tier(Some(clock.clone()));
    let r = router(&spy);
    assert_eq!(answered_by(ask(&t, &r, Role::Decision, 60)), MAIN, "the fast model failed to start: the main model answers");
    assert_eq!(spy.asked(), [FAST, MAIN]);
    // For five minutes the fast model is not asked at all: a model that cannot start is not started at every call.
    *clock.lock().unwrap() += Duration::from_secs(299);
    assert_eq!(answered_by(ask(&t, &r, Role::Fast, 60)), MAIN);
    assert_eq!(spy.asked(), [FAST, MAIN, MAIN]);
    assert!(t.report()["cooling_down_for_seconds"].as_u64().is_some());
    // After that it is tried again, and when it answers it answers.
    *clock.lock().unwrap() += Duration::from_secs(2);
    assert_eq!(answered_by(ask(&t, &r, Role::Diagnosis, 60)), FAST);
    assert_eq!(spy.asked(), [FAST, MAIN, MAIN, FAST]);
    assert!(t.report()["cooling_down_for_seconds"].is_null());
}

#[test]
fn a_fast_model_that_takes_too_long_gets_only_its_own_seconds_and_then_the_main_model_gets_what_is_left() {
    let spy = Spy::new(&[(FAST, vec![Behaviour::Fail(ProviderError::Timeout { idle: false, detail: "timed out".into() })]), (MAIN, vec![Behaviour::Say("m")])]);
    let t = tier(None);
    assert_eq!(answered_by(ask(&t, &router(&spy), Role::Decision, 120)), MAIN);
    let given = spy.deadlines();
    assert_eq!(given[0], Duration::from_secs(20), "the fast model had its twenty seconds and not the call's two minutes");
    assert!(given[1] <= Duration::from_secs(120) && given[1] > Duration::from_secs(100), "the main model had what was left of the call's time: {:?}", given[1]);
    // A call that has less than the fast model's seconds does not give it more.
    let spy = Spy::new(&[(FAST, vec![Behaviour::Say("A")])]);
    ask(&tier(None), &router(&spy), Role::Decision, 5).unwrap();
    assert_eq!(spy.deadlines(), [Duration::from_secs(5)]);
    // A fast model that was too slow is left alone for a while too.
    let spy = Spy::new(&[(FAST, vec![Behaviour::Fail(ProviderError::Timeout { idle: true, detail: "went quiet".into() })]), (MAIN, vec![Behaviour::Say("a"), Behaviour::Say("b")])]);
    let t = tier(None);
    let r = router(&spy);
    ask(&t, &r, Role::Decision, 60).unwrap();
    ask(&t, &r, Role::Decision, 60).unwrap();
    assert_eq!(spy.asked(), [FAST, MAIN, MAIN]);
}

#[test]
fn a_fast_model_that_is_busy_or_whose_window_is_too_small_is_replaced_for_that_call_only() {
    for failure in [
        ProviderError::Overloaded { retry_after: None },
        ProviderError::RateLimited { retry_after: None },
        ProviderError::ContextOverflow { limit: Some(2048), detail: "exceeds the available context size (2048 tokens)".into() },
    ] {
        let spy = Spy::new(&[(FAST, vec![Behaviour::Fail(failure.clone()), Behaviour::Say("fast")]), (MAIN, vec![Behaviour::Say("m")])]);
        let t = tier(None);
        let r = router(&spy);
        assert_eq!(answered_by(ask(&t, &r, Role::Decision, 60)), MAIN, "{failure:?}");
        assert_eq!(answered_by(ask(&t, &r, Role::Decision, 60)), FAST, "{failure:?}: it was not a reason to leave the fast model alone");
        assert_eq!(spy.asked(), [FAST, MAIN, FAST]);
    }
}

#[test]
fn a_refusal_the_persons_stop_and_an_unreadable_answer_are_not_a_reason_to_ask_another_model() {
    for failure in [
        ProviderError::Rejected("the language model refused this request".into()),
        ProviderError::Cancelled,
        ProviderError::InvalidOutput("not JSON".into()),
        ProviderError::Yielded,
        ProviderError::Auth("no".into()),
    ] {
        assert!(!escalates(&failure), "{failure:?}");
        let spy = Spy::new(&[(FAST, vec![Behaviour::Fail(failure.clone())]), (MAIN, vec![Behaviour::Say("must not be asked")])]);
        let t = tier(None);
        let e = ask(&t, &router(&spy), Role::Decision, 60).unwrap_err();
        assert_eq!(e, failure);
        assert_eq!(spy.asked(), [FAST], "{failure:?}: the main model is not asked");
        assert!(t.report()["cooling_down_for_seconds"].is_null(), "and the fast model is not put away for it");
    }
}

#[test]
fn an_answer_that_has_begun_is_never_asked_for_again() {
    let spy = Spy::new(&[(FAST, vec![Behaviour::SayThenFail("The answer is", ProviderError::Unavailable { detail: "the server died".into() })]), (MAIN, vec![Behaviour::Say("a second answer would say it again")])]);
    let e = ask(&tier(None), &router(&spy), Role::Decision, 60).unwrap_err();
    assert!(matches!(e, ProviderError::Unavailable { .. }));
    assert_eq!(spy.asked(), [FAST]);
}

#[test]
fn when_both_fail_the_error_is_the_main_models_and_the_status_names_what_went_wrong_with_the_fast_one() {
    let spy = Spy::new(&[(FAST, vec![Behaviour::Fail(ProviderError::Unavailable { detail: "no".into() })]), (MAIN, vec![Behaviour::Fail(ProviderError::Rejected("main refused".into()))])]);
    let t = tier(None);
    let e = ask(&t, &router(&spy), Role::Decision, 60).unwrap_err();
    assert_eq!(e, ProviderError::Rejected("main refused".into()));
    assert_eq!(t.report()["last_reason_for_asking_the_main_model"], "unavailable");
}

#[test]
fn the_status_is_one_value_that_names_files_and_roles_and_says_what_is_not_built() {
    let spy = Spy::new(&[(FAST, vec![Behaviour::Say("A")])]);
    let t = tier(None);
    ask(&t, &router(&spy), Role::Decision, 60).unwrap();
    let r = t.report();
    assert_eq!((r["switch"].clone(), r["in_use"].clone(), r["fast_model"].clone(), r["main_model"].clone()), ("on".into(), true.into(), FAST.into(), MAIN.into()));
    assert_eq!(r["roles"], serde_json::json!(["decision", "fast", "diagnosis"]));
    assert!(r["answered_by_the_fast_model"].as_u64().unwrap() >= 1);
    assert_eq!(r["built"]["the_launch_of_a_second_server"], true);
    assert_eq!(r["built"]["called_by_the_engine"], true);
    assert_eq!(r["built"]["roles_called_by_the_engine"], serde_json::json!(["decision", "fast"]));
    assert!(r["built"]["not_built"].as_str().unwrap().contains("diagnosis"), "what is still not built is said");
    // Off, from the environment: nothing in use, and the reason.
    flags::with_overrides(&[("AQUA_FAST_TIER", None), ("AQUA_FAST_TIER_SECS", None)], || {
        let off = report_now(&installed(), MAIN);
        assert_eq!((off["switch"].clone(), off["in_use"].clone(), off["why_not"].clone()), ("off".into(), false.into(), "AQUA_FAST_TIER is off".into()));
        assert!(off["fast_model"].is_null());
    });
    // A path is never in the status: only file names.
    assert!(!r.to_string().contains('\\') && !r.to_string().contains("C:"), "{r}");
}

/// The register's name for this test is older than the wiring (it held the engine to NO caller while the second server was not built). It now holds the engine to ONE: only `fast_route.rs`
/// makes a `FastTier`, so that the rule of the fall back is not copied, and the bridge reaches it only through `fast_route::tier_for`, which says none with the switch off.
#[test]
fn nothing_in_the_engine_calls_the_fast_tier_yet() {
    fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                rs_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for top in ["core/src", "adapters", "apps/aqua-cli/src"] {
        rs_files(&root.join(top), &mut files);
    }
    let mut callers = Vec::new();
    for f in files.iter().filter(|f| !f.ends_with("fast_tier.rs") && !f.ends_with("fast_tier_tests.rs")) {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        if text.contains("FastTier::new(") || text.contains("FastTier::with_clock(") || text.contains("fast_tier::FastTier") {
            callers.push(f.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string());
        }
    }
    assert_eq!(callers, ["fast_route.rs"], "the one place that makes a fast tier is fast_route.rs, behind the switch; a second caller needs the page and the register to say so");
    // And the bridge asks for it only through tier_for and applies, which are both off with the switch off.
    let bridge = std::fs::read_to_string(root.join("core/src/model/bridge.rs")).unwrap_or_default();
    assert!(bridge.contains("fast_route::tier_for(") && !bridge.contains("FastTier"), "the bridge goes through fast_route");
}
