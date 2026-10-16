//! The fast tier wired to the engine (card OA9-06, task PR-11; second round). **Off unless `AQUA_FAST_TIER` is on.**
//!
//! `fast_tier.rs` decides which model answers which role and falls back to the main model; `modules::llm_router::fast_server` is the second model server that the small model needs. This
//! file joins them, at the one place where the engine's calls meet the model layer (`bridge.rs`):
//!
//! * [`applies`] says whether a role is routed at all: the switch is on and the role is one of the tiered ones (decision, fast, diagnosis). With the switch off it is the one read of a
//!   flag and **nothing else is read, listed, checked or started**, so every request, the command line of the main server and the prompt are what they were.
//! * [`tier_for`] makes the plan for one call: the main model is the file the engine chose (taken exactly as the engine passed it), the small one is `AQUA_FAST_TIER_MODEL`, a file of the
//!   same folder that is checked (`fast_server::check_model`: a real GGUF file, inside the folder after links are followed, not the main model). When there is no usable small model
//!   it says none and the call goes the way it always went.
//! * [`FastServerProvider`] is the provider the router asks for the small model. It sends the request the local provider would send (`llama_server_provider::body_for`, without a
//!   slot to pin), reads the reply the same way, and turns every way the second server can fail into a class the routing rule falls back on: it is missing, not ready, backing off or
//!   refused the request (`Unavailable`), too slow (`Timeout`), or its window is too small (`ContextOverflow`). A reply that cannot be used (empty, or no probabilities for a decision) is
//!   `Unavailable` too, so that the main model answers and the engine does not fall back to `llama-cli` for it.
//!
//! **Not routed:** the agent's planner, chat, edits and summaries (the main model's, whatever the switch says), and the role diagnosis, which nothing in the engine asks for yet.
//! **Not run on a model:** the tests use `aqua_fake_llama` as both servers.

use super::error::{context_overflow, ProviderError};
use super::fast_tier::{self, FastTier};
use super::llama_server_provider::{self as local, body_for, constraint_of, outcome_of};
use super::retry::RetryPolicy;
use super::router::ModelRouter;
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, Flow, ModelRef, Role};
use super::{Egress, Health, LlamaServerProvider, ModelInfo, ModelProvider};
use crate::flags;
use crate::modules::llm_router::fast_server::{self, Failure};
use crate::modules::llm_router::llama_server::{self as server, Reply};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::warn;

/// The provider's name in a [`ModelRef`].
pub const ID: &str = "llama.cpp-fast";

/// The fast tier of one call (the plan and the rule of the fall back), as the bridge holds it.
pub(crate) type Tier = FastTier;

/// Is this role routed through the fast tier? The switch is on and the role is one of the tiered ones.
pub fn applies(role: Role) -> bool {
    fast_tier::is_tiered(role) && flags::is_on(&flags::FAST_TIER)
}

/// The names of the `.gguf` files in a folder (what the plan looks the small model up in).
fn gguf_names(folder: &Path) -> Vec<String> {
    std::fs::read_dir(folder).map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str().map(str::to_string)).filter(|n| n.to_ascii_lowercase().ends_with(".gguf")).collect()).unwrap_or_default()
}

/// What was last said in the log about a small model that cannot be used, so that the same sentence is not written at every call.
static LAST_UNUSED: Mutex<String> = Mutex::new(String::new());

fn say_once(why: &str) {
    let mut last = LAST_UNUSED.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_str() != why {
        warn!("the fast tier is on and is not used: {why}");
        *last = why.to_string();
    }
}

/// The tier for a call of `role` whose main model is the file `main`, or `None`: the switch is off, the role is not routed, or there is no usable small model (the log says why, once).
pub(crate) fn tier_for(role: Role, main: &Path) -> Option<FastTier> {
    if !applies(role) {
        return None;
    }
    let config = fast_tier::Config::from_env();
    let folder = main.parent()?;
    let main_name = main.file_name()?.to_str()?;
    let mut plan = fast_tier::Plan::new(local::ID, &config, &gguf_names(folder), main_name);
    let Some(small) = plan.fast.take() else {
        say_once(&plan.reason);
        return None;
    };
    match fast_server::check_model(folder, &small.model, main) {
        Ok(file) => plan.fast = Some(ModelRef::new(ID, file.to_string_lossy())),
        Err(why) => {
            say_once(&why);
            return None;
        }
    }
    // The main model is the file the engine passed, exactly as it passed it: the local server is asked for the file it has always been asked for.
    plan.main = ModelRef::new(local::ID, main.to_string_lossy());
    Some(FastTier::new(plan, config.secs))
}

/// The router the tiered calls go through: the local provider for the main model and the provider of the second server for the small one. No retry: the second server never says "later".
pub(crate) fn router() -> Arc<ModelRouter> {
    static ROUTER: OnceLock<Arc<ModelRouter>> = OnceLock::new();
    ROUTER.get_or_init(|| Arc::new(ModelRouter::new(vec![Arc::new(LlamaServerProvider::new(None)), Arc::new(FastServerProvider)]).with_retry(RetryPolicy::NONE))).clone()
}

/// How the second server failed, as the class the routing rule falls back on.
fn error_of(failure: Failure) -> ProviderError {
    match failure {
        Failure::Unavailable(detail) => ProviderError::Unavailable { detail },
        Failure::Timeout(detail) => ProviderError::Timeout { idle: false, detail },
        Failure::TooLong(detail) => ProviderError::ContextOverflow { limit: context_overflow(&detail).flatten(), detail },
        Failure::Cancelled => ProviderError::Cancelled,
    }
}

/// The second local model server as a [`ModelProvider`]: one small model, asked through its own process.
pub struct FastServerProvider;

impl ModelProvider for FastServerProvider {
    fn id(&self) -> &str {
        ID
    }

    fn egress(&self) -> Egress {
        Egress::Local
    }

    /// Never listed: it is a way to answer the roles of the fast tier, not a model the person can choose.
    fn list(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    fn chat(&self, model: &str, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        if !req.tools.is_empty() {
            return Err(ProviderError::Unavailable { detail: "the fast tier's model is not given tool definitions".to_string() });
        }
        let mut body = body_for(req);
        // The second server has one slot, and nothing to pin a prompt to.
        if let Some(fields) = body.as_object_mut() {
            fields.remove("id_slot");
        }
        let budget = ctl.deadline;
        let mut last_tick = 0u64;
        let answer = fast_server::request(Path::new(model), &body, budget, &mut |secs| {
            if secs > last_tick {
                last_tick = secs;
                ctl.tick(secs);
            }
            !(ctl.cancelled() || crate::modules::browser_agent::turn_cancelled())
        })
        .map_err(error_of)?;
        let unreadable = |what: &str| ProviderError::Unavailable { detail: format!("the fast tier's model gave {what}") };
        let Some((text, read, cached)) = server::parse_reply(&answer) else { return Err(unreadable("a reply that cannot be read")) };
        let (_, logprobs) = constraint_of(&req.output);
        let reply = Reply {
            timings: server::timings_of(&answer),
            cut_off: server::reply_was_cut_off(&answer),
            prompt_tokens_read: read,
            prompt_tokens_cached: cached,
            first_token_probs: if logprobs.is_some() { server::top_probs_of(&answer) } else { Vec::new() },
            text,
        };
        // A decision is read from the probabilities, and an answer from its text: without what is wanted the main model is asked, and nothing is made of an empty reply.
        if (logprobs.is_some() && reply.first_token_probs.is_empty()) || (logprobs.is_none() && reply.text.trim().is_empty()) {
            return Err(unreadable(if logprobs.is_some() { "no probabilities for a decision" } else { "an empty answer" }));
        }
        let name = Path::new(model).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| model.to_string());
        if !reply.text.is_empty() {
            sink(ChatEvent::TextDelta(reply.text.clone()));
        }
        let mut outcome = outcome_of(reply, &name);
        outcome.model = ModelRef::new(ID, name);
        sink(ChatEvent::Usage(outcome.usage));
        sink(ChatEvent::Finish(outcome.finish));
        Ok(outcome)
    }

    fn health(&self) -> Health {
        if fast_server::is_ready() {
            Health::Ready
        } else {
            Health::Stopped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempWorld;

    #[test]
    fn with_the_switch_off_no_role_is_routed_and_nothing_is_read() {
        flags::with_overrides(&[("AQUA_FAST_TIER", None)], || {
            for role in Role::ALL {
                assert!(!applies(role), "{}", role.name());
                // A path that does not exist: with the switch off it is not even looked at.
                assert!(tier_for(role, Path::new("no/such/folder/Qwen3-8B-Q4_K_M.gguf")).is_none());
            }
        });
    }

    #[test]
    fn with_the_switch_on_only_the_roles_decision_fast_and_diagnosis_are_routed() {
        flags::with_overrides(&[("AQUA_FAST_TIER", Some("on"))], || {
            let routed: Vec<&str> = Role::ALL.iter().filter(|r| applies(**r)).map(|r| r.name()).collect();
            assert_eq!(routed, ["fast", "decision", "diagnosis"]);
            for role in [Role::Agent, Role::Chat, Role::Edit, Role::Summary] {
                assert!(tier_for(role, Path::new("no/such/Qwen3-8B-Q4_K_M.gguf")).is_none(), "{} is the main model's", role.name());
            }
        });
    }

    #[test]
    fn a_small_model_that_is_not_named_missing_broken_or_the_main_model_leaves_the_call_to_the_main_model() {
        let world = TempWorld::new("fast_route_plan");
        let main = world.write("models/llm/Qwen3-8B-Q4_K_M.gguf", "not a model");
        world.write("models/llm/Broken.gguf", "words, not a header");
        let header = {
            let mut b = Vec::new();
            b.extend(b"GGUF");
            b.extend(3u32.to_le_bytes());
            b.extend(0u64.to_le_bytes());
            b.extend(0u64.to_le_bytes());
            b
        };
        world.write("models/llm/Small-0.6B-Q8_0.gguf", &header);
        let named = |name: &str| {
            let _env = crate::test_support::EnvGuard::set(&[("AQUA_FAST_TIER_MODEL", name)]);
            flags::with_overrides(&[("AQUA_FAST_TIER", Some("on"))], || tier_for(Role::Decision, &main))
        };
        for bad in ["Missing.gguf", "Broken.gguf", "Qwen3-8B-Q4_K_M.gguf", "../x.gguf", ""] {
            assert!(named(bad).is_none(), "{bad:?} is not a usable small model");
        }
        let tier = named("Small-0.6B-Q8_0.gguf").expect("a usable small model");
        let plan = tier.plan();
        assert_eq!((plan.main.provider.as_str(), plan.main.model.as_str()), (local::ID, main.to_string_lossy().as_ref()), "the main model is the file the engine passed, as it passed it");
        let fast = plan.fast.clone().expect("a small model");
        assert_eq!(fast.provider, ID);
        assert!(Path::new(&fast.model).is_absolute() && fast.model.ends_with("Small-0.6B-Q8_0.gguf"), "{}", fast.model);
        assert_eq!(plan.chain(Role::Decision).len(), 2);
        assert_eq!(plan.chain(Role::Agent).len(), 1, "the planner is the main model's");
    }

    #[test]
    fn every_way_the_second_server_can_fail_is_a_class_the_routing_rule_falls_back_on() {
        use fast_tier::escalates;
        let overflow = error_of(Failure::TooLong("the language model refused this request (HTTP 400: exceeds the available context size (4096 tokens; the prompt has 5000))".to_string()));
        assert!(matches!(overflow, ProviderError::ContextOverflow { limit: Some(4096), .. }), "{overflow:?}");
        for failure in [Failure::Unavailable("not ready".into()), Failure::Timeout("slow".into()), Failure::TooLong("exceeds the available context size (10 tokens)".into())] {
            assert!(escalates(&error_of(failure.clone())), "{failure:?}");
        }
        assert_eq!(error_of(Failure::Cancelled), ProviderError::Cancelled);
        assert!(!escalates(&error_of(Failure::Cancelled)), "the person's Stop is never asked again");
    }

    #[test]
    fn the_provider_of_the_second_server_is_local_never_listed_and_is_not_given_tools() {
        let p = FastServerProvider;
        assert_eq!((p.id(), p.egress()), (ID, Egress::Local));
        assert!(p.list().is_empty());
        let messages = [super::super::types::Message::user("which?")];
        let tools = [super::super::types::ToolDef { name: "fs.read".into(), description: "reads".into(), parameters: serde_json::json!({"type": "object"}) }];
        let mut req = ChatRequest::new(&messages, 1, super::super::types::Purpose::Decision);
        req.tools = &tools;
        let mut ctl = CallControl::with_deadline(std::time::Duration::from_secs(1));
        let e = p.chat("Small.gguf", &req, &mut ctl, &mut |_| Flow::Continue).unwrap_err();
        assert!(matches!(e, ProviderError::Unavailable { .. }), "{e:?}");
    }
}
