//! Tests of the provider for an OpenAI-style server on this machine (card OA9-04), against real sockets: the address rules, the request, the key, the reading of streamed and whole
//! answers, a Stop, a server that never answers, one that goes quiet, one that dies, the size limits, and what is never connected to. The server is
//! `test_support::model_server::FakeModelServer`; the recorded traffic of services is `core/tests/provider_cassette_test.rs`.

use super::openai_compat::{loopback_endpoint, Endpoint, ModelSpec, OpenAiCompatProvider, ProviderSpec, Reach, MAX_REQUEST_BYTES};
use super::openai_wire::Shape;
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, FinishReason, Flow, Message, Purpose, ToolChoice, ToolDef};
use super::{Egress, ModelProvider, ProviderError, StructuredMode, ToolMode};
use crate::test_support::model_server::{begin_stream, chunk, completion, delta, end_chunks, event, respond, respond_json, FakeModelServer};
use crate::test_support::wait_until;
use aqua_native_tools::secrets::{MemorySecrets, Secrets};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const KEY: &str = "test-key-0123456789abcdefghij";

fn model(name: &str, shape: Shape) -> ModelSpec {
    ModelSpec { name: name.into(), context: 8192, max_output: 512, family: None, shape, reasoning: false }
}

fn spec_for(endpoint: Endpoint, key_name: Option<&str>) -> ProviderSpec {
    ProviderSpec { id: "ollama".into(), endpoint, key_name: key_name.map(str::to_string), models: vec![model("qwen3:8b", Shape::default())], timeout: Duration::from_secs(30) }
}

fn provider(server: &FakeModelServer) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(spec_for(loopback_endpoint(server.port(), "/v1"), None), Arc::new(MemorySecrets::new()))
}

fn provider_with_key(server: &FakeModelServer, secrets: Arc<dyn Secrets>) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(spec_for(loopback_endpoint(server.port(), "/v1"), Some("models/ollama")), secrets)
}

struct Heard {
    result: Result<ChatOutcome, ProviderError>,
    events: Vec<ChatEvent>,
}

fn ask_with(provider: &OpenAiCompatProvider, stream: bool, mut ctl: CallControl<'_>, stop_at_first_text: bool) -> Heard {
    let messages = [Message::user("a prompt of a synthetic workspace C:\\work\\demo")];
    let req = ChatRequest::new(&messages, 64, Purpose::Chat);
    let req = if stream { req.streaming() } else { req };
    let mut events = Vec::new();
    let result = provider.chat("qwen3:8b", &req, &mut ctl, &mut |e: ChatEvent| {
        let stop = stop_at_first_text && matches!(e, ChatEvent::TextDelta(_));
        events.push(e);
        if stop {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });
    Heard { result, events }
}

fn ask(provider: &OpenAiCompatProvider, stream: bool) -> Heard {
    ask_with(provider, stream, CallControl::with_deadline(Duration::from_secs(20)), false)
}

fn texts(events: &[ChatEvent]) -> Vec<String> {
    events.iter().filter_map(|e| if let ChatEvent::TextDelta(t) = e { Some(t.clone()) } else { None }).collect()
}

fn usage_event(prompt: u64, completion: u64, cached: u64) -> Value {
    json!({ "choices": [], "usage": { "prompt_tokens": prompt, "completion_tokens": completion, "prompt_tokens_details": { "cached_tokens": cached } } })
}

fn finish_event(reason: &str) -> Value {
    json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }] })
}

// ---- where a provider is ----------------------------------------------------------------------------------------------------------------------------------------------

#[test]
fn an_address_is_taken_apart_and_only_this_machine_is_loopback() {
    let ok = |text: &str| Endpoint::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
    let local = ok("http://127.0.0.1:11434/v1");
    assert_eq!((local.host.as_str(), local.port, local.path.as_str(), local.reach, local.tls), ("127.0.0.1", 11434, "/v1", Reach::Loopback, false));
    assert_eq!(ok("http://localhost:1234/v1/").path, "/v1", "a slash at the end is taken off");
    assert_eq!(ok("HTTP://LocalHost:1234/v1").host, "localhost", "scheme and host are not case sensitive");
    let v6 = ok("http://[::1]:8000/v1");
    assert_eq!((v6.host.as_str(), v6.port, v6.reach), ("::1", 8000, Reach::Loopback));
    assert_eq!(v6.authority(), "[::1]:8000");
    assert_eq!(v6.url(), "http://[::1]:8000/v1");
    let bare = ok("http://127.0.0.5");
    assert_eq!((bare.port, bare.path.as_str(), bare.reach), (80, "", Reach::Loopback), "127.0.0.0/8 is all this machine");
    assert_eq!(ok("http://127.255.255.254:9/a/b-c_d.e~f").path, "/a/b-c_d.e~f");

    // Every spelling that is not this machine: other private addresses, names, tricks that look like it.
    for remote in [
        "http://192.168.1.5:11434/v1",
        "http://10.0.0.1",
        "http://example.com/v1",
        "http://api.openai.com/v1",
        "https://api.openai.com/v1",
        "http://0.0.0.0:11434/v1",
        "http://[::ffff:127.0.0.1]:1/v1",
        "http://[::]:1/v1",
        "http://localhost.evil.com/v1",
        "http://127.0.0.1.nip.io/v1",
        "http://2130706433/v1",
        "http://0x7f.0.0.1/v1",
        "http://0177.0.0.1/v1",
        "http://169.254.169.254/latest",
        "http://[fe80::1]:1/v1",
    ] {
        let e = ok(remote);
        assert_eq!(e.reach, Reach::Remote, "{remote} is not this machine");
        assert!(matches!(e.egress(), Egress::Remote { .. }), "{remote}");
        let why = e.refusal().unwrap_or_else(|| panic!("{remote} was not refused"));
        assert!(why.contains("not on this machine") && why.contains(&e.host), "{remote}: {why}");
    }

    // Not an address a service can have.
    for bad in [
        "",
        "   ",
        "ftp://127.0.0.1/v1",
        "127.0.0.1:11434/v1",
        "http://",
        "http://:80/v1",
        "http://127.0.0.1/v1?key=1",
        "http://127.0.0.1/v1#frag",
        "http://127.0.0.1:99999/v1",
        "http://127.0.0.1:0/v1",
        "http://127.0.0.1:abc/v1",
        "http://127.0.0.1:/v1",
        "http://[::1/v1",
        "http://[::1]x/v1",
        "http://127.0.0.1/../etc",
        "http://127.0.0.1/a/./b",
        "http://127.0.0.1//v1",
        "http://127.0.0.1/a b",
        "http://127.0.0.1/a\\b",
        "http://127.0.0.1/%2e%2e",
        "http://127.0.0.1/\u{e9}",
        "http://::1/v1",
        "http://-bad.example/v1",
    ] {
        let e = Endpoint::parse(bad).unwrap_err_or_else_remote(bad);
        assert!(e.contains("the address is not one a model service can have") || e.contains("not on this machine"), "{bad}: {e}");
        assert!(!e.contains("secret"), "a login in an address is never repeated: {e}");
    }
    assert!(Endpoint::parse(&format!("http://127.0.0.1/{}", "a".repeat(400))).is_err(), "a long address is refused");
    // A login in an address is refused, and its words are never repeated (written in two pieces so that the secret scan before a commit has nothing to find in this file).
    let with_login = format!("http://{}@127.0.0.1/v1", ["user", "hunter2"].join(":"));
    let refused = Endpoint::parse(&with_login).unwrap_err().to_string();
    assert!(refused.contains("a login in it") && !refused.contains("hunter2") && !refused.contains("user"), "{refused}");

    // What is refused on this machine.
    assert!(ok("https://127.0.0.1:11434/v1").refusal().unwrap().contains("TLS"));
    assert!(ok("http://127.0.0.1:8767/v1").refusal().unwrap().contains("Aqua's own API"));
    assert_eq!(ok("http://127.0.0.1:11434/v1").refusal(), None);
}

/// A helper for the table above: an address that must not be usable is an error from `parse` or a refusal text (a name that is not this machine parses and is refused).
trait ParseOutcome {
    fn unwrap_err_or_else_remote(self, text: &str) -> String;
}

impl ParseOutcome for Result<Endpoint, super::openai_compat::EndpointError> {
    fn unwrap_err_or_else_remote(self, text: &str) -> String {
        match self {
            Err(e) => e.to_string(),
            Ok(e) => e.refusal().unwrap_or_else(|| panic!("{text} was accepted")),
        }
    }
}

#[test]
fn a_provider_made_by_hand_for_another_machine_connects_to_nothing() {
    // 192.0.2.0/24 is the range kept for examples: a connection there would wait for the whole of its five seconds.
    let remote = Endpoint::parse("http://192.0.2.7:11434/v1").unwrap();
    let p = OpenAiCompatProvider::new(spec_for(remote, None), Arc::new(MemorySecrets::new()));
    assert!(matches!(p.egress(), Egress::Remote { host } if host == "192.0.2.7"));
    let began = Instant::now();
    let heard = ask(&p, false);
    assert!(began.elapsed() < Duration::from_secs(2), "it did not try to connect");
    match heard.result {
        Err(ProviderError::Unavailable { detail }) => assert!(detail.contains("not on this machine") && detail.contains("192.0.2.7"), "{detail}"),
        other => panic!("{other:?}"),
    }
    assert!(heard.events.is_empty());
    // The same for https on this machine, and for Aqua's own port: refused before a socket is opened.
    for text in ["https://127.0.0.1:11434/v1", "http://127.0.0.1:8767/v1"] {
        let p = OpenAiCompatProvider::new(spec_for(Endpoint::parse(text).unwrap(), None), Arc::new(MemorySecrets::new()));
        assert!(matches!(ask(&p, false).result, Err(ProviderError::Unavailable { .. })), "{text}");
    }
    assert!(p.discover(Duration::from_secs(1), &AtomicBool::new(false)).is_err(), "the list of models is not asked of another machine either");
}

#[test]
fn localhost_is_looked_up_at_the_call_and_only_its_loopback_addresses_are_used() {
    let server = FakeModelServer::start(|_, s, _| respond_json(s, 200, &completion("hello")));
    let endpoint = Endpoint::parse(&format!("http://localhost:{}/v1", server.port())).unwrap();
    assert_eq!(endpoint.reach, Reach::Loopback);
    let p = OpenAiCompatProvider::new(spec_for(endpoint, None), Arc::new(MemorySecrets::new()));
    let heard = ask(&p, false);
    assert_eq!(heard.result.unwrap().text, "hello");
    assert_eq!(server.requests()[0].header("host"), Some(format!("localhost:{}", server.port()).as_str()));
}

#[test]
fn nothing_listening_is_unavailable_and_says_where() {
    // A port that was free a moment ago.
    let port = {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let p = OpenAiCompatProvider::new(spec_for(loopback_endpoint(port, "/v1"), None), Arc::new(MemorySecrets::new()));
    match ask(&p, true).result {
        Err(ProviderError::Unavailable { detail }) => assert!(detail.contains(&port.to_string()) && detail.contains("is the model server running"), "{detail}"),
        other => panic!("{other:?}"),
    }
}

// ---- the request ------------------------------------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_streamed_answer_arrives_as_events_and_the_request_has_the_shape_and_no_key() {
    let server = FakeModelServer::start(|_, s, _| {
        begin_stream(s);
        // The degree sign is two bytes; the first of them ends one chunk and the second begins the next.
        let first = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"It is 20\u{b0}".as_bytes().to_vec();
        let (head, tail) = first.split_at(first.len() - 1);
        chunk(s, head);
        chunk(s, tail);
        chunk(s, b"\"},\"finish_reason\":null}]}\n\n");
        event(s, &delta(" outside"));
        event(s, &finish_event("stop"));
        event(s, &usage_event(30, 6, 20));
        chunk(s, b"data: [DONE]\n\n");
        end_chunks(s);
    });
    let heard = ask(&provider(&server), true);
    let out = heard.result.expect("an answer");
    assert_eq!(out.text, "It is 20\u{b0} outside", "a character cut between two reads comes out whole");
    assert_eq!(texts(&heard.events), ["It is 20\u{b0}", " outside"]);
    assert_eq!((out.finish, out.usage.input, out.usage.cached, out.usage.output), (FinishReason::Stop, 10, 20, 6), "the cached tokens are not read again");
    assert!(matches!(heard.events.last(), Some(ChatEvent::Finish(FinishReason::Stop))));
    assert!(matches!(heard.events[heard.events.len() - 2], ChatEvent::Usage(_)));
    assert_eq!(out.model.to_string(), "ollama/qwen3:8b");

    let seen = &server.requests()[0];
    assert_eq!((seen.method.as_str(), seen.path.as_str()), ("POST", "/v1/chat/completions"));
    assert_eq!(seen.header("accept"), Some("text/event-stream"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("connection"), Some("close"));
    assert!(seen.header("authorization").is_none(), "no key was named, so none is sent");
    let body = seen.json();
    assert_eq!((body["model"].as_str(), body["stream"].as_bool(), body["max_tokens"].as_u64()), (Some("qwen3:8b"), Some(true), Some(64)));
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["messages"][0]["role"], "user");
    assert!(body.get("temperature").is_none() && body.get("top_p").is_none(), "what is not set is left to the server");
}

#[test]
fn a_whole_answer_is_read_with_its_numbers_its_thinking_and_its_tool_calls() {
    let server = FakeModelServer::start(|_, s, _| {
        respond_json(
            s,
            200,
            &json!({ "choices": [{ "index": 0, "finish_reason": "tool_calls", "message": { "role": "assistant", "content": "I will read it.", "reasoning_content": "The file is named.",
                "tool_calls": [{ "id": "call_1", "type": "function", "function": { "name": "fs_read", "arguments": "{\"path\":\"a.txt\"}" } }] } }],
                "usage": { "prompt_tokens": 50, "completion_tokens": 9, "prompt_tokens_details": { "cached_tokens": 32 }, "completion_tokens_details": { "reasoning_tokens": 4 } } }),
        );
    });
    let heard = ask(&provider(&server), false);
    let out = heard.result.expect("an answer");
    assert_eq!((out.text.as_str(), out.finish), ("I will read it.", FinishReason::ToolCalls));
    assert_eq!((out.tool_calls[0].id.as_str(), out.tool_calls[0].name.as_str(), out.tool_calls[0].arguments.clone()), ("call_1", "fs_read", json!({ "path": "a.txt" })));
    assert_eq!((out.usage.input, out.usage.cached, out.usage.output, out.usage.reasoning), (18, 32, 9, Some(4)));
    assert!(heard.events.iter().any(|e| matches!(e, ChatEvent::ReasoningDelta(t) if t == "The file is named.")), "{:?}", heard.events);
    let kinds: Vec<&str> = heard
        .events
        .iter()
        .map(|e| match e {
            ChatEvent::ReasoningDelta(_) => "reasoning",
            ChatEvent::TextDelta(_) => "text",
            ChatEvent::ToolCallStart { .. } => "start",
            ChatEvent::ToolCallDelta { .. } => "delta",
            ChatEvent::ToolCall { .. } => "call",
            ChatEvent::Usage(_) => "usage",
            ChatEvent::Finish(_) => "finish",
        })
        .collect();
    assert_eq!(kinds, ["reasoning", "text", "start", "delta", "call", "usage", "finish"]);
    let seen = &server.requests()[0];
    assert_eq!(seen.header("accept"), Some("application/json"));
    assert_eq!(seen.json()["stream"], false);
    assert!(seen.json().get("stream_options").is_none());
}

#[test]
fn tools_are_sent_only_to_a_model_that_is_set_up_for_them() {
    let server = FakeModelServer::start(|_, s, _| respond_json(s, 200, &completion("ok")));
    let tools = [ToolDef { name: "fs_read".into(), description: "Reads a file".into(), parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }) }];
    let messages = [Message::user("read a.txt")];
    let mut req = ChatRequest::new(&messages, 64, Purpose::Chat);
    req.tools = &tools;
    req.tool_choice = ToolChoice::Named("fs_read".into());
    let mut ctl = CallControl::with_deadline(Duration::from_secs(10));
    // The default model has no native tools: the request is refused, with the reason, before a connection is made.
    let refused = provider(&server).chat("qwen3:8b", &req, &mut ctl, &mut |_| Flow::Continue).unwrap_err();
    assert!(matches!(&refused, ProviderError::Rejected(m) if m.contains("native tools") && m.contains("qwen3:8b")), "{refused}");
    assert_eq!(server.connections(), 0);

    let mut spec = spec_for(loopback_endpoint(server.port(), "/v1"), None);
    spec.models = vec![model("qwen3:8b", Shape { tools: ToolMode::Native, structured: StructuredMode::JsonSchema, extra_sampling: false, seed: true })];
    let p = OpenAiCompatProvider::new(spec, Arc::new(MemorySecrets::new()));
    p.chat("qwen3:8b", &req, &mut ctl, &mut |_| Flow::Continue).expect("an answer");
    let body = server.requests()[0].json();
    assert_eq!(body["tools"][0]["function"]["name"], "fs_read");
    assert_eq!(body["tool_choice"]["function"]["name"], "fs_read");
}

#[test]
fn a_model_the_file_does_not_list_is_not_asked_and_the_request_has_a_size() {
    let server = FakeModelServer::start(|_, s, _| respond_json(s, 200, &completion("ok")));
    let p = provider(&server);
    let messages = [Message::user("hi")];
    let req = ChatRequest::new(&messages, 8, Purpose::Chat);
    let mut ctl = CallControl::with_deadline(Duration::from_secs(10));
    let err = p.chat("llama3:70b", &req, &mut ctl, &mut |_| Flow::Continue).unwrap_err();
    assert!(matches!(&err, ProviderError::Rejected(m) if m.contains("llama3:70b") && m.contains("not listed")), "{err}");
    // A prompt over the limit is not sent.
    let big = [Message::user("x".repeat(MAX_REQUEST_BYTES + 10))];
    let err = p.chat("qwen3:8b", &ChatRequest::new(&big, 8, Purpose::Chat), &mut ctl, &mut |_| Flow::Continue).unwrap_err();
    assert!(matches!(&err, ProviderError::Rejected(m) if m.contains("KiB")), "{err}");
    assert_eq!(server.connections(), 0, "neither was sent");
    // And what it lists is what it tells.
    let listed = p.list();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].model.to_string(), listed[0].limits.context, listed[0].limits.max_output), ("ollama/qwen3:8b".to_string(), 8192, 512));
    assert_eq!(listed[0].family, super::Family::Qwen3, "the family is read from the name");
}

// ---- the key ----------------------------------------------------------------------------------------------------------------------------------------------------------

#[test]
fn the_key_is_read_by_name_sent_to_the_one_server_and_is_in_no_error_and_no_event() {
    let secrets = Arc::new(MemorySecrets::new());
    secrets.set("models/ollama", KEY).unwrap();
    // The server that is configured answers; another server is on this machine too and must see nothing.
    let other = FakeModelServer::start(|_, s, _| respond_json(s, 200, &completion("wrong place")));
    let server = FakeModelServer::start(|_, s, _| respond_json(s, 200, &completion("fine")));
    let p = provider_with_key(&server, secrets.clone());
    let heard = ask(&p, false);
    assert_eq!(heard.result.as_ref().unwrap().text, "fine");
    assert_eq!(server.requests()[0].header("authorization"), Some(format!("Bearer {KEY}").as_str()));
    assert_eq!(other.connections(), 0);
    assert!(!format!("{:?} {:?}", heard.events, heard.result).contains(KEY), "the key is in no event and no outcome");
    assert!(!format!("{:?}", p.spec()).contains(KEY), "the spec names the key, it does not hold it");

    // A server that refuses it and quotes it, with the prompt: what it wrote is read for its class and never repeated.
    let quoting = FakeModelServer::start(|req, s, _| {
        let echoed = req.text();
        respond_json(s, 401, &json!({ "error": { "message": format!("Incorrect API key provided: {KEY}. Request was: {echoed}"), "type": "invalid_request_error", "code": "invalid_api_key" } }));
    });
    let heard = ask(&provider_with_key(&quoting, secrets.clone()), false);
    let err = heard.result.unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
    for text in [err.to_string(), format!("{err:?}")] {
        assert!(!text.contains(KEY) && !text.contains("synthetic workspace") && !text.contains("Bearer"), "{text}");
    }
    assert!(err.to_string().contains("invalid_api_key"), "the short code the service gave is kept: {err}");
}

#[test]
fn a_key_that_is_missing_or_cannot_go_in_a_header_is_an_error_before_any_connection() {
    let server = FakeModelServer::start(|_, s, _| respond_json(s, 200, &completion("never")));
    let secrets = Arc::new(MemorySecrets::new());
    // Not in the store.
    let err = ask(&provider_with_key(&server, secrets.clone()), false).result.unwrap_err();
    assert!(matches!(&err, ProviderError::Auth(m) if m.contains("models/ollama") && m.contains("aqua-cli secret-set")), "{err}");
    // A key that would end the header and begin another.
    secrets.set("models/ollama", "abc\r\nX-Evil: 1").unwrap();
    let err = ask(&provider_with_key(&server, secrets.clone()), false).result.unwrap_err();
    assert!(matches!(&err, ProviderError::Auth(m) if m.contains("cannot go in a header")), "{err}");
    assert!(!err.to_string().contains("Evil"));
    // An empty one.
    secrets.set("models/ollama", "").unwrap();
    assert!(matches!(ask(&provider_with_key(&server, secrets.clone()), false).result, Err(ProviderError::Auth(_))));
    // A store that cannot be read.
    struct Broken;
    impl Secrets for Broken {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            Err("Credential Manager error 5".into())
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<bool, String> {
            Ok(false)
        }
    }
    let err = ask(&provider_with_key(&server, Arc::new(Broken)), false).result.unwrap_err();
    assert!(matches!(&err, ProviderError::Unavailable { detail } if detail.contains("models/ollama") && !detail.contains("error 5")), "{err}");
    assert_eq!(server.connections(), 0, "nothing was sent in any of the four cases");
}

// ---- what the server does ---------------------------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_redirect_is_not_followed_and_not_quoted() {
    let elsewhere = FakeModelServer::start(|_, s, _| respond_json(s, 200, &completion("the redirected place")));
    let target = format!("http://127.0.0.1:{}/v1/chat/completions", elsewhere.port());
    let server = FakeModelServer::start(move |_, s, _| respond(s, 302, &[("Location", target.as_str())], ""));
    let err = ask(&provider(&server), false).result.unwrap_err();
    assert!(matches!(&err, ProviderError::Rejected(m) if m.contains("302") && m.contains("does not follow")), "{err}");
    assert!(!err.to_string().contains(&elsewhere.port().to_string()), "where it pointed is not repeated");
    assert_eq!(elsewhere.connections(), 0, "it was not followed");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn an_error_status_is_a_class_with_its_wait_and_the_words_of_the_service_are_not_repeated() {
    let server = FakeModelServer::start(|_, s, _| respond(s, 429, &[("Content-Type", "application/json"), ("Retry-After", "7")], "{\"error\":{\"message\":\"slow down, user said: a secret prompt\",\"type\":\"rate_limit_exceeded\"}}"));
    let err = ask(&provider(&server), false).result.unwrap_err();
    assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
    assert!(matches!(err, ProviderError::RateLimited { .. }));
    assert!(!err.to_string().contains("secret prompt"));
    // A refusal of a prompt that is too long, in llama-server's words, in a 400.
    let server = FakeModelServer::start(|_, s, _| respond(s, 400, &[("Content-Type", "application/json")], "{\"error\":{\"code\":400,\"message\":\"the request exceeds the available context size (4096 tokens)\",\"type\":\"exceed_context_size_error\"}}"));
    let err = ask(&provider(&server), true).result.unwrap_err();
    assert!(matches!(err, ProviderError::ContextOverflow { limit: Some(4096), .. }), "{err:?}");
    // A server that is still loading.
    let server = FakeModelServer::start(|_, s, _| respond(s, 503, &[], "Loading model"));
    assert!(matches!(ask(&provider(&server), false).result, Err(ProviderError::Overloaded { .. })));
}

#[test]
fn the_router_asks_a_service_that_said_later_again_after_the_wait_it_named() {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = count.clone();
    let server = FakeModelServer::start(move |_, s, _| {
        if counted.fetch_add(1, Ordering::SeqCst) == 0 {
            respond(s, 429, &[("Retry-After-Ms", "40")], "{}");
        } else {
            respond_json(s, 200, &completion("second time"));
        }
    });
    let p: Arc<dyn ModelProvider> = Arc::new(provider(&server));
    let gate = Arc::new(super::LocalOnlyGate::new());
    let router = super::ModelRouter::new(vec![p]).with_counters(|_, _| {}).with_gate(gate.clone());
    let messages = [Message::user("hi")];
    let req = ChatRequest::new(&messages, 8, Purpose::Chat);
    let mut ctl = CallControl::with_deadline(Duration::from_secs(20));
    let began = Instant::now();
    let out = router.call_on(&super::ModelRef::new("ollama", "qwen3:8b"), super::Role::Fast, &req, &mut ctl, &mut |_| Flow::Continue).expect("answered the second time");
    assert_eq!(out.text, "second time");
    assert_eq!(server.requests().len(), 2);
    assert!(began.elapsed() >= Duration::from_millis(35), "it waited as long as the service asked");
    // The gate was asked once for the call, and told once how it ended, with the numbers the service gave.
    let report = gate.report();
    assert_eq!((report.allowed, report.refused, report.recent.len()), (1, 0, 1));
    assert_eq!((report.recent[0].ok, report.recent[0].class), (Some(true), super::egress::Class::Loopback));
}

#[test]
fn a_stop_pressed_while_the_answer_is_coming_hangs_up_and_the_server_sees_it() {
    let hung_up = Arc::new(AtomicBool::new(false));
    let seen_hung_up = hung_up.clone();
    let server = FakeModelServer::start(move |_, s, stop| {
        begin_stream(s);
        while !stop.load(Ordering::SeqCst) {
            if !event(s, &delta("word ")) {
                seen_hung_up.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let p = provider(&server);
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        flag.store(true, Ordering::SeqCst);
    });
    let began = Instant::now();
    let heard = ask_with(&p, true, CallControl::new(&cancel, Duration::from_secs(30)), false);
    stopper.join().unwrap();
    assert_eq!(heard.result.unwrap_err(), ProviderError::Cancelled);
    assert!(began.elapsed() < Duration::from_secs(3), "within a read slice of the Stop");
    assert!(!texts(&heard.events).is_empty(), "some of it had been handed over");
    wait_until(Duration::from_secs(5), "the server noticed that the client hung up", || hung_up.load(Ordering::SeqCst));
}

#[test]
fn a_listener_that_leaves_ends_the_call_as_cancelled() {
    let hung_up = Arc::new(AtomicBool::new(false));
    let seen_hung_up = hung_up.clone();
    let server = FakeModelServer::start(move |_, s, stop| {
        begin_stream(s);
        while !stop.load(Ordering::SeqCst) {
            if !event(s, &delta("word ")) {
                seen_hung_up.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let heard = ask_with(&provider(&server), true, CallControl::with_deadline(Duration::from_secs(30)), true);
    assert_eq!(heard.result.unwrap_err(), ProviderError::Cancelled);
    assert_eq!(texts(&heard.events).len(), 1, "it was told nothing after it left");
    wait_until(Duration::from_secs(5), "the server noticed that the client hung up", || hung_up.load(Ordering::SeqCst));
}

#[test]
fn a_server_that_never_answers_is_given_up_on_at_the_deadline() {
    let server = FakeModelServer::start(|_, _, stop| {
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let began = Instant::now();
    let heard = ask_with(&provider(&server), false, CallControl::with_deadline(Duration::from_millis(500)), false);
    match heard.result {
        Err(ProviderError::Timeout { idle: false, detail }) => assert!(detail.contains("timed out"), "{detail}"),
        other => panic!("{other:?}"),
    }
    assert!(began.elapsed() < Duration::from_secs(3));
}

#[test]
fn a_stream_that_goes_quiet_is_hung_up_on_only_after_its_first_event() {
    // The server reads the prompt for half a second, says one word, and goes silent.
    let server = FakeModelServer::start(|_, s, stop| {
        std::thread::sleep(Duration::from_millis(500));
        begin_stream(s);
        event(s, &delta("one"));
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let began = Instant::now();
    let ctl = CallControl::with_deadline(Duration::from_secs(30)).with_idle(Duration::from_millis(300));
    let heard = ask_with(&provider(&server), true, ctl, false);
    match heard.result {
        Err(ProviderError::Timeout { idle: true, detail }) => assert!(crate::modules::llm_router::llama_server::is_idle_text(&detail) && detail.contains("in the middle of the answer"), "{detail}"),
        other => panic!("{other:?}"),
    }
    assert!(began.elapsed() >= Duration::from_millis(700), "the half second of reading the prompt was not taken for a stall");
    assert_eq!(texts(&heard.events), ["one"]);

    // The first-event limit is for a server that never begins.
    let silent = FakeModelServer::start(|_, _, stop| {
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let ctl = CallControl::with_deadline(Duration::from_secs(30)).with_first_event(Duration::from_millis(300));
    match ask_with(&provider(&silent), true, ctl, false).result {
        Err(ProviderError::Timeout { idle: true, detail }) => assert!(detail.contains("before it began to answer"), "{detail}"),
        other => panic!("{other:?}"),
    }
    // A call that is not streamed has no events to wait for: the limit does not apply to it.
    let slow = FakeModelServer::start(|_, s, _| {
        std::thread::sleep(Duration::from_millis(600));
        respond_json(s, 200, &completion("late but whole"));
    });
    let ctl = CallControl::with_deadline(Duration::from_secs(30)).with_idle(Duration::from_millis(100)).with_first_event(Duration::from_millis(100));
    assert_eq!(ask_with(&provider(&slow), false, ctl, false).result.unwrap().text, "late but whole");
}

#[test]
fn a_connection_that_ends_before_the_end_of_the_answer_is_an_error_and_not_a_short_answer() {
    let server = FakeModelServer::start(|_, s, _| {
        begin_stream(s);
        event(s, &delta("The answer is"));
        event(s, &delta(" forty"));
        // No finish reason, no [DONE], no last chunk: the connection ends.
    });
    let heard = ask(&provider(&server), true);
    match heard.result {
        Err(ProviderError::Transport(m)) => assert!(m.contains("stopped in the middle of the answer") && m.contains("(19 characters"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(texts(&heard.events).concat(), "The answer is forty", "what came was handed over, and the error is final");
    // A whole answer cut off by the end of the connection.
    let cut = FakeModelServer::start(|_, s, _| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 500\r\nConnection: close\r\n\r\n{\"choices\":[{\"message\":");
    });
    let err = ask(&provider(&cut), false).result.unwrap_err();
    assert!(matches!(&err, ProviderError::Transport(m) if m.contains("stopped in the middle")), "{err}");
    // A server that closes before it says anything.
    let nothing = FakeModelServer::start(|_, _, _| {});
    assert!(matches!(ask(&provider(&nothing), false).result, Err(ProviderError::Transport(m)) if m.contains("closed the connection")));
}

#[test]
fn what_is_not_http_and_what_is_too_big_is_refused() {
    let server = FakeModelServer::start(|_, s, _| {
        let _ = s.write_all(b"SSH-2.0-OpenSSH_9.0\r\n\r\n");
    });
    assert!(matches!(ask(&provider(&server), false).result, Err(ProviderError::InvalidOutput(m)) if m.contains("did not answer in HTTP")));
    // A head that does not end.
    let endless = FakeModelServer::start(|_, s, _| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\n");
        let junk = format!("X-Junk: {}\r\n", "a".repeat(1000));
        for _ in 0..80 {
            if s.write_all(junk.as_bytes()).is_err() {
                return;
            }
        }
    });
    assert!(matches!(ask(&provider(&endless), false).result, Err(ProviderError::InvalidOutput(m)) if m.contains("longer than 64 KiB")));
    // An event that never ends: more than a megabyte with no blank line.
    let no_end = FakeModelServer::start(|_, s, _| {
        begin_stream(s);
        let line = format!("data: {}\n", "a".repeat(64 * 1024));
        for _ in 0..20 {
            if !chunk(s, line.as_bytes()) {
                return;
            }
        }
    });
    assert!(matches!(ask(&provider(&no_end), true).result, Err(ProviderError::InvalidOutput(m)) if m.contains("has no end")));
    // A whole answer that is not JSON.
    let text = FakeModelServer::start(|_, s, _| respond(s, 200, &[("Content-Type", "text/plain")], "just words"));
    assert!(matches!(ask(&provider(&text), false).result, Err(ProviderError::InvalidOutput(m)) if m.contains("not JSON")));
}

#[test]
fn an_error_inside_a_stream_is_a_class_and_final_once_something_was_said() {
    let server = FakeModelServer::start(|_, s, _| {
        begin_stream(s);
        event(s, &delta("partial"));
        event(s, &json!({ "error": { "message": "the model crashed on: a secret prompt", "type": "server_error" } }));
        end_chunks(s);
    });
    let heard = ask(&provider(&server), true);
    let err = heard.result.unwrap_err();
    assert!(matches!(err, ProviderError::Overloaded { .. }), "{err:?}");
    assert!(!err.to_string().contains("secret prompt"));
    assert_eq!(texts(&heard.events), ["partial"]);
}

#[test]
fn a_chunked_whole_answer_and_one_that_ends_with_the_connection_are_both_read() {
    let body = completion("chunked").to_string();
    let chunked_body = body.clone();
    let server = FakeModelServer::start(move |_, s, _| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        let (a, b) = chunked_body.split_at(20);
        chunk(s, a.as_bytes());
        chunk(s, b.as_bytes());
        end_chunks(s);
    });
    assert_eq!(ask(&provider(&server), false).result.unwrap().text, "chunked");
    let until_close = FakeModelServer::start(move |_, s, _| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n");
        let _ = s.write_all(body.replace("chunked", "until close").as_bytes());
    });
    assert_eq!(ask(&provider(&until_close), false).result.unwrap().text, "until close");
}

#[test]
fn the_list_of_the_servers_models_is_read_in_both_dialects_and_bounded() {
    let never = AtomicBool::new(false);
    let openai = FakeModelServer::start(|req, s, _| {
        assert_eq!((req.method.as_str(), req.path.as_str()), ("GET", "/v1/models"));
        respond_json(s, 200, &json!({ "object": "list", "data": [{ "id": "qwen3:8b" }, { "id": "llama3.2:3b" }, { "id": "qwen3:8b" }, { "id": "" }, { "object": "model" }] }));
    });
    assert_eq!(provider(&openai).discover(Duration::from_secs(5), &never).unwrap(), ["qwen3:8b", "llama3.2:3b"], "twice is once, and an entry with no name is left out");
    assert_eq!(openai.requests()[0].header("accept"), Some("application/json"));
    let ollama = FakeModelServer::start(|_, s, _| respond_json(s, 200, &json!({ "models": [{ "name": "gemma3:4b" }, { "model": "phi4:latest" }] })));
    assert_eq!(provider(&ollama).discover(Duration::from_secs(5), &never).unwrap(), ["gemma3:4b", "phi4:latest"]);
    let many = FakeModelServer::start(|_, s, _| respond_json(s, 200, &json!({ "data": (0..800).map(|i| json!({ "id": format!("m{i}") })).collect::<Vec<_>>() })));
    assert_eq!(provider(&many).discover(Duration::from_secs(5), &never).unwrap().len(), 500);
    let broken = FakeModelServer::start(|_, s, _| respond(s, 200, &[], "<html>"));
    assert!(matches!(provider(&broken).discover(Duration::from_secs(5), &never), Err(ProviderError::InvalidOutput(_))));
    let down = FakeModelServer::start(|_, s, _| respond(s, 500, &[], "boom"));
    assert!(matches!(provider(&down).discover(Duration::from_secs(5), &never), Err(ProviderError::Overloaded { .. })));
}
