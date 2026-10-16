//! Tests of what card OA9-03 changes in the engine's calls, through a scripted model (none of them starts a model): which file each role asks, what each family is sent and how its reply is
//! cleaned, and what the sampling layers put in the request. The request a Qwen3 file is sent with every switch off is the one the engine has always built, and these tests hold it.

use super::adapter::POLICY_TESTS;
use super::*;
use crate::flags;
use crate::modules::llm_router::LlamaRouterEngine;
use crate::test_support::TempWorld;
use serde_json::json;
use std::sync::Arc;

const QWEN: &str = "Qwen3-8B-Q4_K_M.gguf";
const LLAMA: &str = "Llama-3.2-3B-Q4_K_M.gguf";

/// A folder `llm/` with these files (each only has to be a file) next to the place where `profiles.json` and `router.json` live, an engine over it and a scripted router.
fn setup(files: &[&str], script: Vec<Script>) -> (TempWorld, LlamaRouterEngine, Arc<MockProvider>, Arc<ModelRouter>) {
    let world = TempWorld::new("model_profiles_engine");
    for f in files {
        world.write(&format!("llm/{f}"), b"not a model");
    }
    let engine = LlamaRouterEngine::new(world.path("llm")).unwrap();
    let mock = Arc::new(MockProvider::new(script).named(llama_server_provider::ID));
    let router = Arc::new(ModelRouter::new(vec![mock.clone()]));
    (world, engine, mock, router)
}

/// Runs `f` with the model router on for this thread, `router` as its router, and these switches (`AQUA_MODEL_PROFILES`, `AQUA_SAMPLING_PRESETS`, `AQUA_SEED`).
fn run<R>(router: Arc<ModelRouter>, profiles: Option<&str>, presets: Option<&str>, seed: Option<&str>, f: impl FnOnce() -> R) -> R {
    flags::with_overrides(&[("AQUA_MODEL_ROUTER", Some("on")), ("AQUA_MODEL_PROFILES", profiles), ("AQUA_SAMPLING_PRESETS", presets), ("AQUA_SEED", seed)], || with_test_router(router, f))
}

fn plan(engine: &LlamaRouterEngine) -> String {
    engine.generate_agent_response_constrained("Goal: read a.txt", 200, None, |_| {}).unwrap()
}

#[test]
fn a_qwen3_file_is_sent_the_request_it_has_always_been_sent_with_the_switches_off_and_with_the_family_switch_on() {
    let _one = POLICY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    for profiles in [None, Some("on")] {
        let (_world, engine, mock, router) = setup(&[QWEN], vec![Script::reply("<think>plan</think>\n{\"final\":\"done\"}")]);
        let text = run(router, profiles, None, None, || plan(&engine));
        assert_eq!(text, "{\"final\":\"done\"}", "profiles {profiles:?}");
        let seen = mock.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].flattened, "Goal: read a.txt\n/no_think", "profiles {profiles:?}: the switch is still at the end of the prompt");
        assert_eq!(seen[0].reasoning, ReasoningMode::ServerDefault, "profiles {profiles:?}: the request still says what the server was started with");
        assert!(seen[0].sampling.is_empty(), "profiles {profiles:?}: nothing is sent that was not sent before, so the server's 0.3 and 0.9 stand");
        assert!(seen[0].model.ends_with(QWEN));
    }
}

#[test]
fn a_llama_file_is_spoken_to_as_qwen3_with_the_switch_off_and_as_a_llama_with_it_on() {
    let _one = POLICY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let own_words = "Use <reasoning>the tag</reasoning> as the outer element.";
    // Off: the one recipe of every model, and the old cleaning, which cuts the element out of the answer.
    let (_world, engine, mock, router) = setup(&[LLAMA], vec![Script::reply(own_words)]);
    let off = run(router, None, None, None, || plan(&engine));
    assert_ne!(off, own_words, "the old rule cut a reasoning element out of the answer");
    let seen = mock.seen();
    assert_eq!((seen[0].flattened.as_str(), seen[0].reasoning), ("Goal: read a.txt\n/no_think", ReasoningMode::ServerDefault));
    // On: no /no_think, no thinking field, and the answer's own words kept.
    let (_world, engine, mock, router) = setup(&[LLAMA], vec![Script::reply(own_words)]);
    let on = run(router, Some("on"), None, None, || plan(&engine));
    assert_eq!(on, own_words);
    let seen = mock.seen();
    assert_eq!((seen[0].flattened.as_str(), seen[0].reasoning), ("Goal: read a.txt", ReasoningMode::NoSwitch));
    assert!(seen[0].sampling.is_empty());
    // The same body, at the server's door: the thinking field is not there and the rest of the body is unchanged.
    let messages = [Message::user(seen[0].flattened.as_str())];
    let req = ChatRequest::new(&messages, 200, Purpose::Planner).with_reasoning(seen[0].reasoning);
    let body = llama_server_provider::body_for(&req);
    assert!(body.get("chat_template_kwargs").is_none(), "{body}");
    assert_eq!((body["temperature"].clone(), body["top_p"].clone()), (json!(0.3), json!(0.9)));
}

#[test]
fn the_family_is_found_from_the_profile_then_the_header_then_the_name_through_the_engine() {
    let _one = POLICY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    // A file named like a Llama that the person says is a Qwen3: it is spoken to as one.
    let world = TempWorld::new("model_family_engine");
    world.write(&format!("llm/{LLAMA}"), b"not a model");
    world.write("profiles.json", format!(r#"{{"profiles": [{{"match": "{LLAMA}", "family": "qwen3"}}]}}"#));
    // The file is read when the engine starts.
    let engine = LlamaRouterEngine::new(world.path("llm")).unwrap();
    let mock = Arc::new(MockProvider::new(vec![Script::reply("{\"final\":\"a\"}")]).named(llama_server_provider::ID));
    let router = Arc::new(ModelRouter::new(vec![mock.clone()]));
    run(router, Some("on"), None, None, || plan(&engine));
    assert_eq!(mock.seen()[0].flattened, "Goal: read a.txt\n/no_think");
}

#[test]
fn the_sampling_layers_reach_the_request_only_with_their_switches_on_and_the_planner_never_gets_the_chat_preset() {
    let _one = POLICY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let world = TempWorld::new("model_sampling_engine");
    world.write(&format!("llm/{QWEN}"), b"not a model");
    world.write("profiles.json", r#"{"profiles": [{"match": "qwen3*", "sampling": {"temperature": 0.2, "top_k": 30}}], "roles": {"agent": {"top_p": 0.5}}}"#);
    let engine = LlamaRouterEngine::new(world.path("llm")).unwrap();
    let ask = |profiles: Option<&str>, presets: Option<&str>, seed: Option<&str>| {
        let mock = Arc::new(MockProvider::new(vec![Script::reply("{\"final\":\"a\"}"), Script::pieces(&["Hi."])]).named(llama_server_provider::ID));
        let router = Arc::new(ModelRouter::new(vec![mock.clone()]));
        run(router, profiles, presets, seed, || {
            plan(&engine);
            engine.generate_chat_streaming_with_model("Say hi", QWEN, |_| true).unwrap();
        });
        let seen = mock.seen();
        (seen[0].sampling.clone(), seen[1].sampling.clone())
    };
    // Everything off: nothing named.
    let (agent, chat) = ask(None, None, None);
    assert!(agent.is_empty() && chat.is_empty());
    // The profile and the role's own layer.
    let (agent, chat) = ask(Some("on"), None, None);
    assert_eq!((agent.temperature, agent.top_p, agent.top_k), (Some(0.2), Some(0.5), Some(30)), "the profile under the role");
    assert_eq!((chat.temperature, chat.top_p, chat.top_k), (Some(0.2), None, Some(30)), "chat has no layer of its own in the file");
    // The preset alone: the card's numbers for chat and nothing for the planner.
    let (agent, chat) = ask(None, Some("on"), None);
    assert!(agent.is_empty(), "the planner keeps its steady numbers");
    assert_eq!((chat.temperature, chat.top_p, chat.top_k, chat.min_p), (Some(0.7), Some(0.8), Some(20), Some(0.0)));
    // A seed alone is only a seed, on both.
    let (agent, chat) = ask(None, None, Some("7"));
    assert_eq!(agent, Sampling { seed: Some(7), ..Sampling::default() });
    assert_eq!(chat, agent);
}

#[test]
fn pressing_use_on_a_model_makes_the_planner_and_chat_ask_that_file_and_the_decision_follows() {
    let (_world, engine, mock, router) = setup(&[QWEN, LLAMA], vec![Script::reply("{\"final\":\"a\"}"), Script::pieces(&["Hi."]), Script::reply("{\"final\":\"b\"}"), Script::pieces(&["Hi again."])]);
    // Before: the Qwen3-8B preference decides for the agent, and chat follows it.
    run(router.clone(), None, None, None, || {
        plan(&engine);
        engine.generate_chat_streaming("Say hi", |_| true).unwrap();
    });
    // The person presses Use on the Llama.
    engine.use_model(LLAMA).unwrap();
    run(router, None, None, None, || {
        plan(&engine);
        engine.generate_chat_streaming("Say hi", |_| true).unwrap();
    });
    let models: Vec<String> = mock.seen().iter().map(|s| s.model.rsplit(['\\', '/']).next().unwrap_or("").to_string()).collect();
    assert_eq!(models, [QWEN, QWEN, LLAMA, LLAMA], "the regression of D-PR-2: the model that was pressed is the one that answers");
    assert_eq!(engine.active_model_filename(), LLAMA);
    assert_eq!(engine.agent_model_filename(), LLAMA);
    assert_eq!(engine.model_filename_for(Role::Decision), LLAMA);
}

#[test]
fn a_role_set_to_a_model_changes_only_that_role_and_a_deleted_model_is_forgotten() {
    let (world, engine, _mock, _router) = setup(&[QWEN, LLAMA], vec![]);
    engine.assign_roles(&[(Role::Fast, Some(LLAMA.to_string()))]).unwrap();
    assert_eq!((engine.model_filename_for(Role::Fast).as_str(), engine.model_filename_for(Role::Agent).as_str(), engine.model_filename_for(Role::Chat).as_str()), (LLAMA, QWEN, QWEN));
    assert!(engine.role_warnings().iter().any(|w| w.contains("fast uses")), "{:?}", engine.role_warnings());
    // Both choices are in the file, written by the engine, and a new engine over the same folder reads them back.
    let again = LlamaRouterEngine::new(world.path("llm")).unwrap();
    assert_eq!(again.model_filename_for(Role::Fast), LLAMA);
    // A name that is not a file of the folder is refused and nothing changes.
    assert!(engine.assign_roles(&[(Role::Chat, Some("Nothing.gguf".into()))]).is_err());
    assert_eq!(engine.model_filename_for(Role::Chat), QWEN);
    // Deleting the model takes it out of the roles that named it.
    engine.remove_model(LLAMA).unwrap();
    assert_eq!(engine.model_filename_for(Role::Fast), QWEN);
    assert!(!std::fs::read_to_string(world.path("router.json")).unwrap().contains("Llama"));
}
