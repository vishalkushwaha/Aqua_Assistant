//! The wire format of an OpenAI-style chat completion: the body of a request, and the reading of an answer (card OA9-04, task PR-07 of the OpenCode audit).
//!
//! Ollama, LM Studio, vLLM, an `llama-server` run by hand and the OpenAI API itself speak the same dialect of `POST /v1/chat/completions`. This file knows that dialect and nothing
//! else: no socket, no key, no rule about where a request may go (that is `openai_compat.rs` and the egress gate).
//!
//! * [`chat_body`] writes the JSON of a request from a [`ChatRequest`] and what the model can take ([`Shape`]): the messages, `max_tokens`, the sampling that is set, the seed, the
//!   stop words, `stream` with `stream_options.include_usage`, native tools when the model has them, and the shape the reply must have (`response_format` with a JSON schema, a plain
//!   JSON object, or the server's `grammar`) **when the model can be held to it**; when it cannot, nothing is sent for it and the caller's tolerant reader of the reply is what
//!   holds (see `documentation/MODEL_PROVIDER.md`, "What a model that cannot be held to a schema gets").
//! * [`OpenAiReader`] reads the events of a streamed answer, or a whole answer: the text, the thinking (`reasoning_content`, or `reasoning`), tool calls assembled by their `index`
//!   (the id and the name come with the first delta, the arguments in pieces, parsed once at the end), the finish reason, and the numbers the service measured (llama.cpp's
//!   `timings`, else the `usage` of the OpenAI shape). It was written for the cassette replay of card OA9-05 and moved here when the provider that uses it was written; the cassette
//!   tests still run it against recorded bytes.

use super::error::{classify_http, ProviderError};
use super::types::{ChatEvent, ChatOutcome, ChatRequest, FinishReason, Flow, MessageRole, ModelRef, Output, Sampling, ToolCall, ToolChoice, Usage};
use super::{StructuredMode, ToolMode};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::time::SystemTime;

/// The most stop words one request carries (the OpenAI API takes four; the servers on this machine take more, and a long list is a sign of a mistake).
const MAX_STOP_WORDS: usize = 16;

/// What a model can take, for the body of a request: the entry of the model in `providers.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Does the model take tool definitions and give tool calls? Without it a request that has tools is refused, and one that has none is sent with none.
    pub tools: ToolMode,
    /// How the reply can be held to a shape.
    pub structured: StructuredMode,
    /// Send `top_k`, `min_p` and `repeat_penalty` when the request sets them. The OpenAI API has none of them, llama-server and LM Studio do, and a server that does not know a field
    /// may refuse the request, so the person says so per provider.
    pub extra_sampling: bool,
    /// Send the seed when the request has one.
    pub seed: bool,
}

impl Default for Shape {
    fn default() -> Self {
        Shape { tools: ToolMode::None, structured: StructuredMode::None, extra_sampling: false, seed: true }
    }
}

fn number(v: f64) -> Option<Value> {
    v.is_finite().then(|| json!(v))
}

/// The sampling that is set, as the fields of the body (and only that: what is not set is what the server's own settings say).
fn sampling_fields(sampling: &Sampling, shape: &Shape, body: &mut Map<String, Value>) {
    if let Some(v) = sampling.temperature.and_then(number) {
        body.insert("temperature".into(), v);
    }
    if let Some(v) = sampling.top_p.and_then(number) {
        body.insert("top_p".into(), v);
    }
    if shape.seed {
        if let Some(seed) = sampling.seed {
            body.insert("seed".into(), json!(seed));
        }
    }
    let stop: Vec<&String> = sampling.stop.iter().filter(|s| !s.is_empty()).take(MAX_STOP_WORDS).collect();
    if !stop.is_empty() {
        body.insert("stop".into(), json!(stop));
    }
    if shape.extra_sampling {
        if let Some(k) = sampling.top_k {
            body.insert("top_k".into(), json!(k));
        }
        if let Some(v) = sampling.min_p.and_then(number) {
            body.insert("min_p".into(), v);
        }
        if let Some(v) = sampling.repeat_penalty.and_then(number) {
            body.insert("repeat_penalty".into(), v);
        }
    }
}

/// The JSON body of a chat-completions request for `model`.
///
/// Errors are refusals of the request as it is (`ProviderError::Rejected`, with the reason in a sentence): a request with tools for a model that has none, a decision (which needs the
/// probabilities of the first token, which this provider does not read), a request with nothing to say.
pub fn chat_body(model: &str, req: &ChatRequest<'_>, shape: &Shape) -> Result<Value, ProviderError> {
    if !req.tools.is_empty() && shape.tools != ToolMode::Native {
        return Err(ProviderError::Rejected(format!("the model {model} is not set up for native tools in providers.json (tools: \"native\"), so a request that has tool definitions is not sent")));
    }
    if matches!(req.output, Output::Choice(_)) {
        return Err(ProviderError::Rejected("a decision needs the probabilities of the model's first token, which the OpenAI-compatible provider does not read yet".into()));
    }
    let mut messages: Vec<Value> = Vec::new();
    let system: Vec<&str> = req.system.iter().copied().filter(|s| !s.trim().is_empty()).collect();
    if !system.is_empty() {
        messages.push(json!({ "role": "system", "content": system.join("\n\n") }));
    }
    for m in req.messages {
        // A tool result has no call id to answer here (the request type carries none), so it is told to the model as what it is, by a person's turn that says so, as the local model is told.
        let (role, content) = match m.role {
            MessageRole::System => ("system", m.content.clone()),
            MessageRole::User => ("user", m.content.clone()),
            MessageRole::Assistant => ("assistant", m.content.clone()),
            MessageRole::Tool => ("user", format!("Tool result: {}", m.content)),
        };
        messages.push(json!({ "role": role, "content": content }));
    }
    if messages.is_empty() {
        return Err(ProviderError::Rejected("the request has no message to send".into()));
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_tokens".into(), json!(req.max_tokens.max(1)));
    body.insert("stream".into(), json!(req.stream));
    if req.stream {
        // The number of tokens comes in the last event, or not at all.
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    sampling_fields(&req.sampling, shape, &mut body);

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req.tools.iter().map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.parameters } })).collect();
        body.insert("tools".into(), Value::Array(tools));
        body.insert(
            "tool_choice".into(),
            match &req.tool_choice {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Named(name) => json!({ "type": "function", "function": { "name": name } }),
            },
        );
    }

    match &req.output {
        Output::Text | Output::Choice(_) => {}
        Output::JsonSchema(schema) => match shape.structured {
            StructuredMode::JsonSchema | StructuredMode::Gbnf => {
                body.insert("response_format".into(), json!({ "type": "json_schema", "json_schema": { "name": "aqua_action", "strict": true, "schema": schema } }));
            }
            StructuredMode::JsonObject => {
                body.insert("response_format".into(), json!({ "type": "json_object" }));
            }
            // Nothing is sent: the prompt says what shape the reply has and the tolerant reader of the caller takes what comes.
            StructuredMode::None => {}
        },
        Output::Grammar(grammar) => {
            if shape.structured == StructuredMode::Gbnf {
                body.insert("grammar".into(), json!(grammar));
            }
        }
    }
    Ok(Value::Object(body))
}

/// Did the request ask for a shape the model could not be held to (so that what comes back is held only by the prompt)? For the log and the answer of a test call.
pub fn shape_not_enforced(req: &ChatRequest<'_>, shape: &Shape) -> bool {
    match &req.output {
        Output::Text | Output::Choice(_) => false,
        Output::JsonSchema(_) => shape.structured == StructuredMode::None,
        Output::Grammar(_) => shape.structured != StructuredMode::Gbnf,
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Reading an answer
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// One tool call as its deltas add up.
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
}

/// Reads the events of an OpenAI-style chat completion (llama-server, Ollama, the OpenAI API): the text, the thinking (`reasoning_content` or `reasoning`), tool calls assembled by their
/// `index` (the id and the name come with the first delta, the arguments in pieces), the finish reason, and the numbers the service measured (llama.cpp's `timings`, else `usage`).
pub(crate) struct OpenAiReader<'a> {
    sink: &'a mut dyn FnMut(ChatEvent) -> Flow,
    pub(crate) text: String,
    calls: BTreeMap<u64, PartialCall>,
    usage: Usage,
    pub(crate) finish: Option<FinishReason>,
    pub(crate) done: bool,
}

impl<'a> OpenAiReader<'a> {
    pub(crate) fn new(sink: &'a mut dyn FnMut(ChatEvent) -> Flow) -> Self {
        Self { sink, text: String::new(), calls: BTreeMap::new(), usage: Usage::default(), finish: None, done: false }
    }

    fn emit(&mut self, event: ChatEvent) -> Result<(), ProviderError> {
        if (self.sink)(event) == Flow::Stop {
            return Err(ProviderError::Cancelled);
        }
        Ok(())
    }

    /// One `data:` payload of the stream.
    pub(crate) fn event(&mut self, data: &str) -> Result<(), ProviderError> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        // Something that is not JSON in a stream is skipped, as the engine skips it.
        let Ok(event) = serde_json::from_str::<Value>(data) else { return Ok(()) };
        if event.get("error").is_some() {
            return Err(classify_http(500, &[], &event.to_string(), SystemTime::now()));
        }
        read_usage(&event, &mut self.usage);
        let Some(choice) = event.pointer("/choices/0") else { return Ok(()) };
        if let Some(piece) = choice.pointer("/delta/content").and_then(Value::as_str).filter(|p| !p.is_empty()) {
            self.text.push_str(piece);
            self.emit(ChatEvent::TextDelta(piece.to_string()))?;
        }
        if let Some(piece) = ["/delta/reasoning_content", "/delta/reasoning"].iter().find_map(|p| choice.pointer(p).and_then(Value::as_str)).filter(|p| !p.is_empty()) {
            self.emit(ChatEvent::ReasoningDelta(piece.to_string()))?;
        }
        if let Some(deltas) = choice.pointer("/delta/tool_calls").and_then(Value::as_array) {
            for delta in deltas {
                self.tool_delta(delta)?;
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(finish_reason(reason));
        }
        Ok(())
    }

    fn tool_delta(&mut self, delta: &Value) -> Result<(), ProviderError> {
        let index = delta.get("index").and_then(Value::as_u64).unwrap_or(0);
        let call = self.calls.entry(index).or_default();
        if let Some(id) = delta.get("id").and_then(Value::as_str).filter(|i| !i.is_empty()) {
            call.id = id.to_string();
        }
        if let Some(name) = delta.pointer("/function/name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
            call.name.push_str(name);
        }
        let fragment = delta.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("").to_string();
        call.arguments.push_str(&fragment);
        let mut events = Vec::new();
        if !call.announced && !call.name.is_empty() {
            call.announced = true;
            if call.id.is_empty() {
                call.id = format!("call_{index}");
            }
            events.push(ChatEvent::ToolCallStart { id: call.id.clone(), name: call.name.clone() });
            // What came before the name was known is told with it.
            if !call.arguments.is_empty() {
                events.push(ChatEvent::ToolCallDelta { id: call.id.clone(), json_fragment: call.arguments.clone() });
            }
        } else if call.announced && !fragment.is_empty() {
            events.push(ChatEvent::ToolCallDelta { id: call.id.clone(), json_fragment: fragment });
        }
        for event in events {
            self.emit(event)?;
        }
        Ok(())
    }

    /// A plain (not streamed) `chat.completion`.
    pub(crate) fn whole(&mut self, body: &[u8]) -> Result<(), ProviderError> {
        let reply: Value = serde_json::from_slice(body).map_err(|_| ProviderError::InvalidOutput("the reply of the model service is not JSON".into()))?;
        if reply.get("error").is_some() && reply.get("choices").is_none() {
            return Err(classify_http(500, &[], &reply.to_string(), SystemTime::now()));
        }
        read_usage(&reply, &mut self.usage);
        let message = reply.pointer("/choices/0/message").cloned().unwrap_or(Value::Null);
        if let Some(thought) = ["reasoning_content", "reasoning"].iter().find_map(|k| message.get(k).and_then(Value::as_str)).filter(|t| !t.is_empty()) {
            self.emit(ChatEvent::ReasoningDelta(thought.to_string()))?;
        }
        if let Some(text) = message.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            self.text.push_str(text);
            self.emit(ChatEvent::TextDelta(text.to_string()))?;
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for (i, call) in calls.iter().enumerate() {
                let mut delta = call.clone();
                delta["index"] = Value::from(i as u64);
                self.tool_delta(&delta)?;
            }
        }
        self.finish = reply.pointer("/choices/0/finish_reason").and_then(Value::as_str).map(finish_reason);
        self.done = true;
        Ok(())
    }

    /// The end: the tool calls whole (their arguments parsed), the numbers, the finish reason, and the outcome.
    pub(crate) fn finish_with(mut self, model: &ModelRef) -> Result<ChatOutcome, ProviderError> {
        let mut tool_calls = Vec::new();
        for call in std::mem::take(&mut self.calls).into_values() {
            if call.name.is_empty() {
                return Err(ProviderError::InvalidOutput("a tool call of the reply has no name".into()));
            }
            let arguments = if call.arguments.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&call.arguments).map_err(|_| ProviderError::InvalidOutput(format!("the arguments of the tool call {} are not JSON", call.name)))?
            };
            self.emit(ChatEvent::ToolCall { id: call.id.clone(), name: call.name.clone(), arguments: arguments.clone() })?;
            tool_calls.push(ToolCall { id: call.id, name: call.name, arguments });
        }
        let finish = self.finish.unwrap_or(if tool_calls.is_empty() { FinishReason::Stop } else { FinishReason::ToolCalls });
        let usage = self.usage;
        self.emit(ChatEvent::Usage(usage))?;
        self.emit(ChatEvent::Finish(finish))?;
        Ok(ChatOutcome { text: self.text, tool_calls, usage, finish, model: model.clone(), choice_probs: Vec::new() })
    }
}

fn finish_reason(word: &str) -> FinishReason {
    match word {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

/// What the service measured, from an event or a reply: llama.cpp's `timings` (what it read, what it took from its cache, what it wrote, and how long each took), else the `usage` of
/// the OpenAI shape (its `prompt_tokens` counts the cached ones too, so what was read is what is left of it).
fn read_usage(event: &Value, into: &mut Usage) {
    if let Some(timings) = event.get("timings").filter(|t| t.get("prompt_n").is_some()) {
        let n = |key: &str| timings[key].as_u64().unwrap_or(0);
        let ms = |key: &str| timings[key].as_f64().filter(|v| v.is_finite() && *v >= 0.0).map(|v| v.round() as u64).unwrap_or(0);
        into.input = n("prompt_n");
        into.cached = n("cache_n");
        into.output = n("predicted_n");
        into.prompt_ms = ms("prompt_ms");
        into.decode_ms = ms("predicted_ms");
    } else if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
        let cached = usage.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64).unwrap_or(0);
        if let Some(prompt) = usage.get("prompt_tokens").and_then(Value::as_u64) {
            into.input = prompt.saturating_sub(cached);
            into.cached = cached;
        }
        if let Some(output) = usage.get("completion_tokens").and_then(Value::as_u64) {
            into.output = output;
        }
    }
    if let Some(reasoning) = event.pointer("/usage/completion_tokens_details/reasoning_tokens").and_then(Value::as_u64) {
        into.reasoning = Some(reasoning);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{Message, Purpose, ToolDef};

    fn body(req: &ChatRequest<'_>, shape: &Shape) -> Value {
        chat_body("qwen3:8b", req, shape).expect("a body")
    }

    fn user() -> [Message; 1] {
        [Message::user("what is in a.txt")]
    }

    #[test]
    fn a_plain_request_has_the_messages_the_room_and_nothing_the_caller_did_not_set() {
        let messages = [Message::user("hello"), Message::assistant("hi"), Message::new(MessageRole::Tool, "42"), Message::system("be brief")];
        let system = ["the stable rules", "   ", "the second part"];
        let req = ChatRequest::new(&messages, 0, Purpose::Chat).with_system(&system);
        let b = body(&req, &Shape::default());
        assert_eq!(b["model"], "qwen3:8b");
        assert_eq!(b["max_tokens"], 1, "no reply has no room: at least one token");
        assert_eq!(b["stream"], false);
        let roles: Vec<(&str, &str)> = b["messages"].as_array().unwrap().iter().map(|m| (m["role"].as_str().unwrap(), m["content"].as_str().unwrap())).collect();
        assert_eq!(roles, [("system", "the stable rules\n\nthe second part"), ("user", "hello"), ("assistant", "hi"), ("user", "Tool result: 42"), ("system", "be brief")], "a blank system part is left out and a tool result is told as one");
        for absent in ["temperature", "top_p", "seed", "stop", "top_k", "min_p", "repeat_penalty", "tools", "tool_choice", "response_format", "grammar", "stream_options", "chat_template_kwargs"] {
            assert!(b.get(absent).is_none(), "{absent} was not asked for");
        }
        let streamed = body(&ChatRequest::new(&messages, 8, Purpose::Chat).streaming(), &Shape::default());
        assert_eq!((streamed["stream"].clone(), streamed["stream_options"]["include_usage"].clone()), (json!(true), json!(true)));
    }

    #[test]
    fn the_sampling_that_is_set_is_sent_and_the_extra_fields_only_where_the_person_said_the_server_takes_them() {
        let messages = user();
        let sampling = Sampling { temperature: Some(0.7), top_p: Some(0.8), top_k: Some(20), min_p: Some(0.0), seed: Some(7), stop: vec!["END".into(), String::new()], repeat_penalty: Some(1.1) };
        let req = ChatRequest::new(&messages, 64, Purpose::Chat).with_sampling(sampling.clone());
        let plain = body(&req, &Shape::default());
        assert_eq!((plain["temperature"].clone(), plain["top_p"].clone(), plain["seed"].clone(), plain["stop"].clone()), (json!(0.7), json!(0.8), json!(7), json!(["END"])));
        for extra in ["top_k", "min_p", "repeat_penalty"] {
            assert!(plain.get(extra).is_none(), "{extra} is not in the OpenAI API, and a server that does not know it may refuse the request");
        }
        let extra = body(&req, &Shape { extra_sampling: true, ..Shape::default() });
        assert_eq!((extra["top_k"].clone(), extra["min_p"].clone(), extra["repeat_penalty"].clone()), (json!(20), json!(0.0), json!(1.1)));
        let no_seed = body(&req, &Shape { seed: false, ..Shape::default() });
        assert!(no_seed.get("seed").is_none(), "a model that takes no seed is sent none");
        // A number that is not a number is not sent.
        let nan = ChatRequest::new(&messages, 8, Purpose::Chat).with_sampling(Sampling { temperature: Some(f64::NAN), ..Sampling::default() });
        assert!(body(&nan, &Shape::default()).get("temperature").is_none());
        // Not more stop words than a mistake.
        let many = ChatRequest::new(&messages, 8, Purpose::Chat).with_sampling(Sampling { stop: (0..40).map(|i| format!("s{i}")).collect(), ..Sampling::default() });
        assert_eq!(body(&many, &Shape::default())["stop"].as_array().unwrap().len(), MAX_STOP_WORDS);
    }

    #[test]
    fn the_shape_a_reply_must_have_is_asked_of_a_model_that_can_be_held_to_it_and_left_to_the_prompt_otherwise() {
        let messages = user();
        let schema = json!({ "type": "object", "properties": { "final": { "type": "string" } } });
        let req = ChatRequest::new(&messages, 64, Purpose::Planner).with_output(Output::JsonSchema(schema.clone()));
        for (structured, expected) in [
            (StructuredMode::JsonSchema, Some("json_schema")),
            (StructuredMode::Gbnf, Some("json_schema")),
            (StructuredMode::JsonObject, Some("json_object")),
            (StructuredMode::None, None),
        ] {
            let shape = Shape { structured, ..Shape::default() };
            let b = body(&req, &shape);
            assert_eq!(b.pointer("/response_format/type").and_then(Value::as_str), expected, "{structured:?}");
            assert_eq!(shape_not_enforced(&req, &shape), expected.is_none(), "{structured:?}: the caller can be told that the prompt is all that holds the reply");
        }
        assert_eq!(body(&req, &Shape { structured: StructuredMode::JsonSchema, ..Shape::default() })["response_format"]["json_schema"]["schema"], schema, "the schema goes as it is");
        let grammar = ChatRequest::new(&messages, 64, Purpose::Planner).with_output(Output::Grammar("root ::= \"A\"".into()));
        assert_eq!(body(&grammar, &Shape { structured: StructuredMode::Gbnf, ..Shape::default() })["grammar"], "root ::= \"A\"");
        assert!(body(&grammar, &Shape { structured: StructuredMode::JsonSchema, ..Shape::default() }).get("grammar").is_none(), "a grammar is llama-server's, and is not sent to a server that has none");
        assert!(shape_not_enforced(&grammar, &Shape { structured: StructuredMode::JsonSchema, ..Shape::default() }));
        assert!(!shape_not_enforced(&ChatRequest::new(&messages, 8, Purpose::Chat), &Shape::default()));
    }

    #[test]
    fn tools_need_a_model_that_takes_them_and_a_decision_needs_what_this_provider_does_not_read() {
        let messages = user();
        let tools = [ToolDef { name: "fs_read".into(), description: "Reads a file".into(), parameters: json!({ "type": "object" }) }];
        let mut req = ChatRequest::new(&messages, 64, Purpose::Planner);
        req.tools = &tools;
        let err = chat_body("qwen3:8b", &req, &Shape::default()).unwrap_err();
        assert!(matches!(&err, ProviderError::Rejected(m) if m.contains("native tools")), "{err}");
        let native = Shape { tools: ToolMode::Native, ..Shape::default() };
        for (choice, expected) in [(ToolChoice::Auto, json!("auto")), (ToolChoice::None, json!("none")), (ToolChoice::Required, json!("required")), (ToolChoice::Named("fs_read".into()), json!({ "type": "function", "function": { "name": "fs_read" } }))] {
            req.tool_choice = choice;
            let b = body(&req, &native);
            assert_eq!(b["tool_choice"], expected);
            assert_eq!(b["tools"][0], json!({ "type": "function", "function": { "name": "fs_read", "description": "Reads a file", "parameters": { "type": "object" } } }));
        }
        let decision = ChatRequest::new(&messages, 1, Purpose::Decision).with_output(Output::Choice(vec!["A".into(), "B".into()]));
        assert!(matches!(chat_body("m", &decision, &Shape::default()), Err(ProviderError::Rejected(m)) if m.contains("probabilities")));
        let empty: [Message; 0] = [];
        assert!(matches!(chat_body("m", &ChatRequest::new(&empty, 8, Purpose::Chat), &Shape::default()), Err(ProviderError::Rejected(m)) if m.contains("no message")));
    }

    /// What a reader said, and how the call ended.
    fn read_stream(events: &[&str]) -> (Result<ChatOutcome, ProviderError>, Vec<ChatEvent>, bool) {
        let mut heard = Vec::new();
        let (result, done) = {
            let mut sink = |e: ChatEvent| {
                heard.push(e);
                Flow::Continue
            };
            let mut reader = OpenAiReader::new(&mut sink);
            let mut failed = None;
            for data in events {
                if let Err(e) = reader.event(data) {
                    failed = Some(e);
                    break;
                }
            }
            let done = reader.done;
            match failed {
                Some(e) => (Err(e), done),
                None => (reader.finish_with(&ModelRef::new("p", "m")), done),
            }
        };
        (result, heard, done)
    }

    #[test]
    fn tool_calls_are_assembled_by_their_index_and_a_call_whose_arguments_are_not_json_is_an_error() {
        let first = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"fs_read","arguments":""}},{"index":1,"function":{"name":"fs_list","arguments":"{\"pa"}}]}}]}"#;
        let second = r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"th\":\".\"}"}},{"index":0,"function":{"arguments":"{\"path\":\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let (result, events, done) = read_stream(&[first, second, "[DONE]"]);
        let out = result.unwrap();
        assert!(done);
        assert_eq!(out.tool_calls.iter().map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.clone())).collect::<Vec<_>>(), [("c1", "fs_read", json!({ "path": "a" })), ("call_1", "fs_list", json!({ "path": "." }))], "an id the service did not give is made from the index");
        assert_eq!(out.finish, FinishReason::ToolCalls);
        assert_eq!(events.iter().filter(|e| matches!(e, ChatEvent::ToolCall { .. })).count(), 2);
        // Arguments that do not parse are the service's mistake, named by the tool and not repeated.
        let broken = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"fs_read","arguments":"{not json, a secret"}}]},"finish_reason":"tool_calls"}]}"#;
        let err = read_stream(&[broken, "[DONE]"]).0.unwrap_err();
        assert!(matches!(&err, ProviderError::InvalidOutput(m) if m.contains("fs_read") && !m.contains("secret")), "{err}");
        // No arguments at all is an empty object.
        let none = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"todo_list"}}]},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(read_stream(&[none]).0.unwrap().tool_calls[0].arguments, json!({}));
        // A call that never got a name.
        let nameless = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#;
        assert!(matches!(read_stream(&[nameless]).0, Err(ProviderError::InvalidOutput(m)) if m.contains("no name")));
    }

    #[test]
    fn the_reader_skips_what_is_not_json_reads_thinking_in_both_spellings_and_stops_at_the_listener() {
        let events = [
            "garbage that is not json",
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"think one"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"think two"}}]}"#,
            r#"{"choices":[{"delta":{"content":"answer"},"finish_reason":"length"}]}"#,
            r#"{"timings":{"prompt_n":10,"cache_n":90,"predicted_n":5,"prompt_ms":123.6,"predicted_ms":45.2}}"#,
        ];
        let (result, heard, done) = read_stream(&events);
        let out = result.unwrap();
        assert!(!done, "there was no [DONE]: the finish reason is what ended it");
        assert_eq!((out.text.as_str(), out.finish, out.cut_off()), ("answer", FinishReason::Length, true));
        assert_eq!((out.usage.input, out.usage.cached, out.usage.output, out.usage.prompt_ms, out.usage.decode_ms), (10, 90, 5, 124, 45), "llama.cpp's own numbers, rounded as the engine rounds them");
        assert_eq!(heard.iter().filter(|e| matches!(e, ChatEvent::ReasoningDelta(_))).count(), 2);
        // A listener that leaves ends it.
        let mut sink = |_: ChatEvent| Flow::Stop;
        let mut reader = OpenAiReader::new(&mut sink);
        assert_eq!(reader.event(r#"{"choices":[{"delta":{"content":"x"}}]}"#), Err(ProviderError::Cancelled));
    }

    #[test]
    fn a_whole_answer_that_is_an_error_is_a_class_and_not_an_empty_answer() {
        let mut sink = |_: ChatEvent| Flow::Continue;
        let mut reader = OpenAiReader::new(&mut sink);
        let err = reader.whole(br#"{"error":{"message":"model not found, a secret prompt","type":"not_found"}}"#).unwrap_err();
        assert!(!err.to_string().contains("secret prompt"), "{err}");
        let mut sink = |_: ChatEvent| Flow::Continue;
        let mut reader = OpenAiReader::new(&mut sink);
        reader.whole(br#"{"choices":[{"message":{"content":"fine"},"finish_reason":"stop"}]}"#).unwrap();
        assert_eq!(reader.finish_with(&ModelRef::new("p", "m")).unwrap().text, "fine");
    }
}
