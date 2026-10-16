//! Tests of the registry of the model servers the person listed (card OA9-04): the switch, the file read again when it changes, a broken file, an address of another machine, the status,
//! and the test call, against a fake model server on a loopback port.

use super::*;
use crate::test_support::model_server::{completion, respond, respond_json, FakeModelServer};
use crate::test_support::TempWorld;
use aqua_native_tools::secrets::MemorySecrets;
use serde_json::json;

const KEY: &str = "registry-key-0123456789abcdef";

fn registry(world: &TempWorld) -> (ProviderRegistry, Arc<MemorySecrets>) {
    let secrets = Arc::new(MemorySecrets::new());
    (ProviderRegistry::open(&world.path("models"), secrets.clone()), secrets)
}

fn file_for(port: u16) -> String {
    format!(
        r#"{{ "version": 1, "providers": {{ "ollama": {{ "base_url": "http://127.0.0.1:{port}/v1", "key_name": "models/ollama",
            "models": {{ "qwen3:8b": {{ "context": 8192 }}, "llama3.2:3b": {{}} }} }} }} }}"#
    )
}

fn ollama() -> FakeModelServer {
    FakeModelServer::start(|req, s, _| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/models") => respond_json(s, 200, &json!({ "data": [{ "id": "qwen3:8b" }, { "id": "gemma3:4b" }] })),
        ("POST", "/v1/chat/completions") => respond_json(s, 200, &completion("OK")),
        _ => respond(s, 404, &[], "not found"),
    })
}

#[test]
fn off_unless_switched_on_and_then_the_file_is_not_even_read() {
    let world = TempWorld::new("providers_off");
    world.write("models/providers.json", "this is not json");
    let (registry, _) = registry(&world);
    let off = registry.status(false);
    assert_eq!((off["enabled"].clone(), off["file"]["state"].clone(), off["providers"].clone()), (json!(false), json!("off"), json!([])));
    assert_eq!(off["switch"], "AQUA_MODEL_PROVIDERS");
    assert!(off["file"]["error"].is_null(), "a file that is not read is not found fault with");
    let err = registry.test(false, "ollama", None).unwrap_err();
    assert!(err.contains("AQUA_MODEL_PROVIDERS=on"), "{err}");
    // On, the same file is what it is.
    assert_eq!(registry.status(true)["file"]["state"], "broken");
    // And off again, at once.
    assert_eq!(registry.status(false)["file"]["state"], "off");
}

#[test]
fn a_provider_added_to_the_file_is_there_on_the_next_call_and_one_taken_out_is_gone_with_no_restart() {
    let world = TempWorld::new("providers_reload");
    let server = ollama();
    let (registry, _) = registry(&world);
    let absent = registry.status(true);
    assert_eq!((absent["enabled"].clone(), absent["file"]["state"].clone(), absent["providers"].clone()), (json!(true), json!("absent"), json!([])));
    assert!(!world.path("models/providers.json").exists(), "a missing file is not written: it is the person's to make");

    world.write("models/providers.json", file_for(server.port()));
    let ok = registry.status(true);
    assert_eq!(ok["file"]["state"], "ok");
    let p = &ok["providers"][0];
    assert_eq!((p["provider"].clone(), p["egress"].clone(), p["key_name"].clone(), p["base_url"].clone()), (json!("ollama"), json!("loopback"), json!("models/ollama"), json!(server.base_url())));
    let models: Vec<&str> = p["models"].as_array().unwrap().iter().map(|m| m["model"].as_str().unwrap()).collect();
    assert_eq!(models, ["ollama/llama3.2:3b", "ollama/qwen3:8b"]);
    assert_eq!(p["models"][1]["context"], 8192);
    assert_eq!((p["models"][1]["tools"].clone(), p["models"][1]["structured"].clone()), (json!("constrained_json"), json!("none")));

    // The router has it at once.
    let router = registry.router(true);
    assert!(router.provider("ollama").is_some());
    assert_eq!(router.list().len(), 2);
    // The same bytes are not read again; other bytes are.
    world.write("models/providers.json", r#"{ "providers": {} }"#);
    assert_eq!(registry.status(true)["providers"], json!([]));
    assert!(registry.router(true).provider("ollama").is_none());
    // Off, the router has nothing.
    world.write("models/providers.json", file_for(server.port()));
    assert!(registry.router(false).provider("ollama").is_none());
}

#[test]
fn a_file_that_cannot_be_used_turns_every_provider_off_and_says_where_without_repeating_what_it_holds() {
    let world = TempWorld::new("providers_broken");
    let server = ollama();
    let (registry, _) = registry(&world);
    // A missing comma on the fourth line.
    world.write("models/providers.json", "{\n  \"version\": 1,\n  \"providers\": {\n    \"ollama\": { \"base_url\": \"http://127.0.0.1:1/v1\" } \"x\": 1\n  }\n}\n");
    let broken = registry.status(true);
    assert_eq!((broken["file"]["state"].clone(), broken["providers"].clone()), (json!("broken"), json!([])));
    assert_eq!((broken["file"]["error"]["kind"].clone(), broken["file"]["error"]["line"].clone()), (json!("syntax"), json!(4)));
    // A key the file does not know: the nearest known one is named, and the whole file is refused.
    world.write("models/providers.json", r#"{ "providerz": {} }"#);
    let unknown = registry.status(true);
    assert_eq!(unknown["file"]["state"], "broken");
    // A rule of the file: a key that is not in the namespace of the models.
    world.write("models/providers.json", r#"{ "providers": { "ollama": { "base_url": "http://127.0.0.1:1/v1", "key_name": "github/token" } } }"#);
    let rule = registry.status(true);
    assert_eq!((rule["file"]["state"].clone(), rule["file"]["error"]["kind"].clone()), (json!("broken"), json!("invalid")));
    assert!(rule["file"]["error"]["message"].as_str().unwrap().contains("models/"));
    assert_eq!(rule["providers"], json!([]), "no provider is used while the file is broken");
    assert!(registry.test(true, "ollama", None).unwrap_err().contains("no provider is called ollama"));
    // Mended, it is read again at once.
    world.write("models/providers.json", file_for(server.port()));
    assert_eq!(registry.status(true)["file"]["state"], "ok");
}

#[test]
fn an_address_of_another_machine_is_shown_with_its_reason_and_is_never_tried() {
    let world = TempWorld::new("providers_remote");
    world.write("models/providers.json", r#"{ "providers": { "far": { "base_url": "https://api.openai.com/v1", "key_name": "models/openai", "models": { "gpt-4o": {} } }, "lan": { "base_url": "http://192.168.1.20:11434/v1" } } }"#);
    let (registry, secrets) = registry(&world);
    secrets.set("models/openai", KEY).unwrap();
    let status = registry.status(true);
    assert_eq!(status["providers"], json!([]));
    let refused = status["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 2);
    assert!(refused.iter().all(|r| r["reason"].as_str().unwrap().contains("not on this machine")), "{refused:?}");
    assert!(status["remote"].as_str().unwrap().starts_with("refused"));
    let err = registry.test(true, "far", Some("gpt-4o")).unwrap_err();
    assert!(err.contains("far is not used") && err.contains("api.openai.com is not on this machine"), "{err}");
    assert!(!status.to_string().contains(KEY), "the key of a refused provider is not read for the status");
    // Nothing was asked, and the gate saw nothing.
    assert_eq!(status["gate"]["allowed"], 0);
}

#[test]
fn the_status_says_whether_a_named_key_is_in_the_store_and_never_what_it_is() {
    let world = TempWorld::new("providers_key");
    let server = ollama();
    world.write("models/providers.json", file_for(server.port()));
    let (registry, secrets) = registry(&world);
    assert_eq!(registry.status(true)["providers"][0]["key_present"], false);
    secrets.set("models/ollama", KEY).unwrap();
    let with = registry.status(true);
    assert_eq!(with["providers"][0]["key_present"], true);
    assert!(!with.to_string().contains(KEY), "the key is in no part of the status");
    assert_eq!(server.connections(), 0, "the status asks the store and not the server");
}

#[test]
fn the_test_call_asks_the_server_for_its_models_and_makes_one_short_call_through_the_gate() {
    let world = TempWorld::new("providers_test");
    let server = ollama();
    world.write("models/providers.json", file_for(server.port()));
    let (registry, secrets) = registry(&world);
    secrets.set("models/ollama", KEY).unwrap();

    let list_only = registry.test(true, "ollama", None).expect("a report");
    assert_eq!(list_only["reachable"], true);
    assert_eq!(list_only["server_models"], json!(["qwen3:8b", "gemma3:4b"]));
    assert_eq!(list_only["not_listed"], json!(["gemma3:4b"]), "the server has a model the file does not list");
    assert_eq!(list_only["missing_on_server"], json!(["llama3.2:3b"]), "the file lists a model the server does not have");
    assert!(list_only.get("chat").is_none(), "no model named, no call made");
    assert_eq!(server.requests().len(), 1);

    let full = registry.test(true, "ollama", Some("qwen3:8b")).expect("a report");
    assert_eq!((full["chat"]["ok"].clone(), full["chat"]["text"].clone(), full["chat"]["model"].clone(), full["chat"]["finish"].clone()), (json!(true), json!("OK"), json!("ollama/qwen3:8b"), json!("Stop")));
    assert_eq!((full["chat"]["tokens_in"].clone(), full["chat"]["tokens_out"].clone()), (json!(12), json!(3)));
    let sent = server.requests().into_iter().filter(|r| r.method == "POST").collect::<Vec<_>>();
    assert_eq!(sent.len(), 1);
    let body = sent[0].json();
    assert_eq!((body["model"].clone(), body["max_tokens"].clone(), body["stream"].clone()), (json!("qwen3:8b"), json!(16), json!(false)));
    assert_eq!(body["messages"][0]["content"], TEST_PROMPT, "the words are the route's own");
    assert_eq!(sent[0].header("authorization"), Some(format!("Bearer {KEY}").as_str()));
    assert!(!full.to_string().contains(KEY));

    // The gate said yes once and was told how it ended.
    let gate = &registry.status(true)["gate"];
    assert_eq!((gate["allowed"].clone(), gate["refused"].clone()), (json!(1), json!(0)));
    assert_eq!((gate["recent"][0]["provider"].clone(), gate["recent"][0]["class"].clone(), gate["recent"][0]["ok"].clone(), gate["recent"][0]["role"].clone()), (json!("ollama"), json!("loopback"), json!(true), json!("fast")));

    // Refusals of the request itself.
    assert!(registry.test(true, "ollama", Some("gemma3:4b")).unwrap_err().contains("not listed under ollama"), "a model the file does not list is not asked, even if the server has it");
    assert!(registry.test(true, "nobody", None).unwrap_err().contains("no provider is called nobody"));
}

#[test]
fn a_server_that_is_not_running_is_a_report_that_says_so_and_a_key_that_is_missing_is_a_failed_call() {
    let world = TempWorld::new("providers_down");
    let port = {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    world.write("models/providers.json", file_for(port));
    let (registry, secrets) = registry(&world);
    secrets.set("models/ollama", KEY).unwrap();
    let report = registry.test(true, "ollama", Some("qwen3:8b")).expect("a report, not a refusal");
    assert_eq!(report["reachable"], false);
    assert_eq!(report["error"]["class"], "unavailable");
    assert!(report["error"]["message"].as_str().unwrap().contains("is the model server running"));
    // The key is named and was never set: the call fails as an Auth error and the server is not asked (the list of models fails the same way).
    let server = ollama();
    secrets.delete("models/ollama").unwrap();
    world.write("models/providers.json", file_for(server.port()));
    let report = registry.test(true, "ollama", Some("qwen3:8b")).unwrap();
    assert_eq!((report["chat"]["ok"].clone(), report["chat"]["class"].clone()), (json!(false), json!("auth")));
    assert_eq!(server.connections(), 0);
}

#[test]
fn the_file_is_read_by_the_loader_of_the_other_config_files() {
    let world = TempWorld::new("providers_loader");
    // Missing: the default, and nothing written.
    let data = world.root.clone();
    match config_files::load(&data, &PROVIDERS) {
        Loaded::Defaulted(file) => assert!(file.providers.is_empty()),
        other => panic!("{other:?}"),
    }
    assert!(!world.path("models/providers.json").exists());
    // Comments and a trailing comma, as in the other files.
    world.write("models/providers.json", "// mine\n{ \"providers\": { \"a\": { \"base_url\": \"http://127.0.0.1:1/v1\", }, } }\n");
    match config_files::load(&data, &PROVIDERS) {
        Loaded::Ok(file) => assert_eq!(file.providers.len(), 1),
        other => panic!("{other:?}"),
    }
    // A broken one says what it turns off, in the words of the others.
    world.write("models/providers.json", "{ \"providers\": ");
    let broken = config_files::load(&data, &PROVIDERS);
    assert!(broken.error().is_some());
    assert_eq!(PROVIDERS.on_broken.effect(), "none of its model servers is asked until it is fixed");
    assert_eq!((PROVIDERS.name, PROVIDERS.rel), ("providers.json", "models/providers.json"));
}
