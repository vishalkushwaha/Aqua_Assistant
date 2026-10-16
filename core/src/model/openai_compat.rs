//! A model server of the OpenAI dialect on this machine, as a [`ModelProvider`] (card OA9-04, task PR-07 of the OpenCode audit): an Ollama, an LM Studio, a vLLM, an `llama-server`
//! started by hand.
//!
//! **What it is allowed to reach.** One address, the one in the person's `providers.json`, and only if that address is on this machine: `127.0.0.1` (any `127.x.x.x`), `[::1]` or
//! `localhost`, in plain `http`. Everything else is refused by [`Endpoint::refusal`] with a sentence that says why, at three places that do not trust each other: when the file is
//! read (`providers_config::resolve`), when the router asks the egress gate (`egress.rs`), and here, before a socket is opened. Nothing in this file can open a connection to another
//! machine, follows a redirect, or sends the key of the provider anywhere but to the one address (a redirect is an error that names the status and nothing else). `localhost` is
//! resolved at the moment of the call and only its loopback addresses are used, so a hosts file that points the name elsewhere reaches nothing.
//!
//! **The key.** A provider may name a key (`key_name`, a name of the credential store, written by `aqua-cli secret-set` and read at the moment of the call, never kept). It goes in
//! one `Authorization: Bearer` header of the one request, is checked to hold only characters that may be in a header, is not part of any error, log line or event this file makes,
//! and is dropped when the call ends.
//!
//! **The connection.** What `llama_server.rs` uses for the local server and nothing new: a `TcpStream` with the HTTP/1.1 written by hand, read in slices of a quarter of a second so
//! that the person's Stop, the deadline and the watch on a stream that has gone quiet are looked at while the server is working. The chunked framing and the events are read by the
//! code of `sse.rs`, which `llama_server.rs` now uses too; what the events mean is `openai_wire.rs`. There is no extra dependency (the lock file is as it was).
//!
//! **What the answer may be.** The request is held to 2 MiB; the head of the answer to 64 KiB, an error body to 16 KiB (and it is read for its class, never repeated: `classify_http`),
//! a whole answer to 8 MiB, a stream to 16 MiB, and one event to 1 MiB. A server that goes over any of these is hung up on.
//!
//! **What it does not do yet.** No `https` (no TLS client is built for model services), no remote service (that needs the gate that cleans the text, asks the person for each call,
//! keeps a spend cap and a ledger, which is not built: the router's [`LocalOnlyGate`](super::egress::LocalOnlyGate) refuses it with that reason), no decisions from probabilities, no
//! tool results with a call id. The model must be listed in `providers.json`: the file is the list of what may be asked.

use super::error::{classify_http, ProviderError};
use super::openai_wire::{chat_body, OpenAiReader, Shape};
use super::sse::{ChunkedDecoder, EventSplitter};
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, Flow, ModelRef};
use super::{Capabilities, Egress, Family, Health, Limits, ModelInfo, ModelProvider, PromptCache, ReasoningSupport, Residency, StructuredMode, ToolMode};
use aqua_native_tools::secrets::Secrets;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// The most a request may weigh. A prompt that is larger is refused here, before anything is sent (the window of the models Aqua runs is a few thousand tokens).
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_ERROR_BODY: usize = 16 * 1024;
const MAX_WHOLE_BODY: usize = 8 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_LISTED_MODELS: usize = 500;
/// How long a connection may take to open, and a write to be taken.
const CONNECT_SECS: u64 = 5;
const WRITE_SECS: u64 = 10;
/// The read slice: how often the person's Stop, the deadline and the quiet watch are looked at while the server works.
const SLICE_MS: u64 = 250;
/// A provider's own time limit when `providers.json` says none.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// A key of the credential store, for the length of one call. It prints as nothing.
struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Where a provider is
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// Where an address is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// This machine: `127.x.x.x`, `::1`, `localhost`.
    Loopback,
    /// Anything else.
    Remote,
}

/// The address of a provider's API (`http://127.0.0.1:11434/v1`), taken apart and classified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The host as written, lower case, without brackets.
    pub host: String,
    pub port: u16,
    /// What comes after the port, with no slash at the end (`/v1`); empty for none. The calls are `{path}/chat/completions` and `{path}/models`.
    pub path: String,
    pub reach: Reach,
    /// The address said `https`.
    pub tls: bool,
}

/// Why an address cannot be a provider's: one sentence for the person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointError(pub String);

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The ports that are Aqua's own and are never a model service: its API (8767, or what `AQUA_API_PORT` says).
fn aquas_own_ports() -> Vec<u16> {
    let mut ports = vec![8767];
    if let Some(p) = std::env::var("AQUA_API_PORT").ok().and_then(|v| v.trim().parse::<u16>().ok()) {
        ports.push(p);
    }
    ports
}

impl Endpoint {
    /// Takes an address apart. A plain `http://` or `https://` address of a host, an optional port and an optional path made of letters, digits and `. _ ~ - /`: no login, no query,
    /// no fragment, no space, no control character, no `..`. Whether the host is on this machine is [`Endpoint::reach`]; an address of another machine is not an error here (the
    /// gate says no, with its own reason), one that is not an address at all is.
    pub fn parse(base_url: &str) -> Result<Endpoint, EndpointError> {
        let bad = |why: &str| Err(EndpointError(format!("the address is not one a model service can have ({why}): write it as http://127.0.0.1:11434/v1")));
        let text = base_url.trim();
        if text.is_empty() || text.len() > 300 {
            return bad("it is empty or longer than 300 characters");
        }
        if !text.is_ascii() || text.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
            return bad("it has a space, a control character, a backslash or a letter that is not ASCII");
        }
        let lower_scheme = text.get(..8).map(str::to_ascii_lowercase).unwrap_or_default();
        let (tls, rest) = if lower_scheme.starts_with("http://") {
            (false, &text[7..])
        } else if lower_scheme == "https://" {
            (true, &text[8..])
        } else {
            return bad("it must start with http:// or https://");
        };
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(authority_end);
        if authority.contains('@') {
            return bad("it has a login in it");
        }
        if tail.contains(['?', '#']) {
            return bad("it has a query or a fragment");
        }
        let (host, port_text) = if let Some(inner) = authority.strip_prefix('[') {
            let Some((host, after)) = inner.split_once(']') else { return bad("the bracket of an IPv6 address is not closed") };
            if !after.is_empty() && !after.starts_with(':') {
                return bad("there is something between the bracket of an IPv6 address and the port");
            }
            (host.to_string(), after.strip_prefix(':').map(str::to_string))
        } else if let Some((host, port)) = authority.rsplit_once(':') {
            (host.to_string(), Some(port.to_string()))
        } else {
            (authority.to_string(), None)
        };
        if host.is_empty() {
            return bad("there is no host");
        }
        let port = match port_text.as_deref() {
            None => {
                if tls {
                    443
                } else {
                    80
                }
            }
            Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => match p.parse::<u16>() {
                Ok(n) if n > 0 => n,
                _ => return bad("the port is not between 1 and 65535"),
            },
            _ => return bad("the port is not a number"),
        };
        let path = tail.trim_end_matches('/').to_string();
        if !path.is_empty() {
            if !path.starts_with('/') || path.contains("//") || path.split('/').any(|seg| seg == ".." || seg == ".") || !path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-' | '/')) {
                return bad("its path has a character or a part (such as ..) that is not allowed");
            }
        }
        let host = host.to_ascii_lowercase();
        let reach = match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) if ip.is_loopback() => Reach::Loopback,
            Ok(IpAddr::V6(ip)) if ip == Ipv6Addr::LOCALHOST => Reach::Loopback,
            Ok(_) => Reach::Remote,
            Err(_) if host == "localhost" => Reach::Loopback,
            Err(_) => {
                // A name is a name: letters, digits, dots and hyphens, as the standard has it. (Whether it is on this machine, it is not.)
                if !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')) || host.starts_with(['.', '-']) {
                    return bad("the host is not a name or an address");
                }
                Reach::Remote
            }
        };
        Ok(Endpoint { host, port, path, reach, tls })
    }

    /// Where it runs, for the router's gate.
    pub fn egress(&self) -> Egress {
        match self.reach {
            Reach::Loopback => Egress::Loopback,
            Reach::Remote => Egress::Remote { host: self.host.clone() },
        }
    }

    /// `host:port`, as a `Host:` header and a message write it (an IPv6 address in brackets).
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The address as the person wrote it, normalised (nothing secret is in it: a login is refused).
    pub fn url(&self) -> String {
        format!("{}://{}{}", if self.tls { "https" } else { "http" }, self.authority(), self.path)
    }

    /// Why this address cannot be used, in one sentence, or `None` when it can: it is on this machine, in plain http, and not one of Aqua's own ports.
    pub fn refusal(&self) -> Option<String> {
        if self.reach == Reach::Remote {
            return Some(format!(
                "{} is not on this machine. Only a model server on this machine (127.0.0.1, ::1 or localhost) can be added: a model on another machine needs the gate that cleans the text, asks you for each call, caps what is spent and keeps a ledger, and that is not built, so it is refused",
                self.host
            ));
        }
        if self.tls {
            return Some("https needs a TLS client, which Aqua does not have for model services: a server on this machine is spoken to in plain http (http://127.0.0.1:11434/v1), and the text does not leave the machine".into());
        }
        if aquas_own_ports().contains(&self.port) {
            return Some(format!("port {} is Aqua's own API and not a model service", self.port));
        }
        None
    }

    /// The socket addresses to try, in order. `localhost` is looked up now and only its loopback addresses are kept; a literal address is itself.
    fn addresses(&self) -> Result<Vec<SocketAddr>, ProviderError> {
        if let Ok(ip) = self.host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, self.port)]);
        }
        let found: Vec<SocketAddr> = (self.host.as_str(), self.port).to_socket_addrs().map(|it| it.filter(|a| a.ip().is_loopback()).collect()).unwrap_or_default();
        if found.is_empty() {
            // The name is looked up and not found, or it leads somewhere else: nothing is sent.
            return Err(ProviderError::Unavailable { detail: format!("the name {} does not lead to this machine (no loopback address), so nothing was sent", self.host) });
        }
        // The IPv4 address first: the servers of local models listen on it (Ollama and LM Studio do not listen on ::1 unless they are told to).
        let mut found = found;
        found.sort_by_key(|a| a.is_ipv6());
        found.dedup();
        Ok(found)
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// What a provider is told
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// One model of a provider, as `providers.json` lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelSpec {
    /// The name the server knows it by (`qwen3:8b`).
    pub name: String,
    /// The window, in tokens, as far as the person knows it.
    pub context: u32,
    /// The most tokens one reply of it is given.
    pub max_output: u32,
    /// The family, when the person says (else it is read from the name).
    pub family: Option<String>,
    pub shape: Shape,
    /// Does it think before it answers (the thinking comes as `reasoning_content`)?
    pub reasoning: bool,
}

/// A provider, as `providers.json` describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderSpec {
    pub id: String,
    pub endpoint: Endpoint,
    /// The name of the key in the credential store, if the server wants one.
    pub key_name: Option<String>,
    pub models: Vec<ModelSpec>,
    /// The longest a call to it may take.
    pub timeout: Duration,
}

/// An OpenAI-compatible server on this machine.
pub struct OpenAiCompatProvider {
    spec: ProviderSpec,
    secrets: Arc<dyn Secrets>,
}

impl OpenAiCompatProvider {
    pub fn new(spec: ProviderSpec, secrets: Arc<dyn Secrets>) -> Self {
        Self { spec, secrets }
    }

    pub fn spec(&self) -> &ProviderSpec {
        &self.spec
    }

    fn model(&self, name: &str) -> Option<&ModelSpec> {
        self.spec.models.iter().find(|m| m.name == name)
    }

    /// The key, read now. `None` when the provider names none. A key that is not in the store, or holds a character that cannot go in a header, is an `Auth` error that says which
    /// name and never what it holds.
    fn key(&self) -> Result<Option<Secret>, ProviderError> {
        let Some(name) = &self.spec.key_name else { return Ok(None) };
        match self.secrets.get(name) {
            Ok(Some(value)) if !value.is_empty() && value.len() <= 4096 && value.chars().all(|c| c.is_ascii_graphic()) => Ok(Some(Secret(value))),
            Ok(Some(_)) => Err(ProviderError::Auth(format!("the key named {name} is empty or holds a character that cannot go in a header"))),
            Ok(None) => Err(ProviderError::Auth(format!("the key named {name} is not in the credential store (set it with: aqua-cli secret-set {name})"))),
            Err(_) => Err(ProviderError::Unavailable { detail: format!("the credential store could not be read for the key named {name}") }),
        }
    }

    /// One request to the server and its answer's head, ready to be read. `payload` is the JSON body of a POST; none is a GET.
    fn open(&self, suffix: &str, payload: Option<&str>, accept: &str, watch: &mut Watch<'_, '_>) -> Result<(Conn, Head), ProviderError> {
        let ep = &self.spec.endpoint;
        let key = self.key()?;
        let stream = connect(ep, watch)?;
        let method = if payload.is_some() { "POST" } else { "GET" };
        let auth = key.as_ref().map(|k| format!("Authorization: Bearer {}\r\n", k.0)).unwrap_or_default();
        let content = match payload {
            Some(p) => format!("Content-Type: application/json\r\nContent-Length: {}\r\n", p.len()),
            None => String::new(),
        };
        let head = format!("{method} {}{suffix} HTTP/1.1\r\nHost: {}\r\nUser-Agent: aqua\r\nAccept: {accept}\r\n{content}{auth}Connection: close\r\n\r\n", ep.path, ep.authority());
        let mut stream = stream;
        let _ = stream.set_write_timeout(Some(Duration::from_secs(WRITE_SECS)));
        let sent = stream.write_all(head.as_bytes()).and_then(|_| match payload {
            Some(p) => stream.write_all(p.as_bytes()),
            None => Ok(()),
        });
        if let Err(e) = sent {
            return Err(ProviderError::Transport(format!("sending the request to {} failed: {}", ep.authority(), e.kind())));
        }
        let _ = stream.flush();
        let _ = stream.set_read_timeout(Some(Duration::from_millis(SLICE_MS)));
        let mut conn = Conn::new(stream);
        let head = conn.read_head(watch)?;
        Ok((conn, head))
    }

    /// The names of the models the server says it has (`GET {base}/models`). Used by the test call of the app and by nothing else: the models a call may ask for are the ones the file lists.
    pub fn discover(&self, deadline: Duration, cancel: &std::sync::atomic::AtomicBool) -> Result<Vec<String>, ProviderError> {
        if let Some(why) = self.spec.endpoint.refusal() {
            return Err(ProviderError::Unavailable { detail: why });
        }
        let mut ctl = CallControl::new(cancel, deadline.min(self.spec.timeout));
        let mut watch = Watch::new(&mut ctl, deadline.min(self.spec.timeout), false);
        let (mut conn, head) = self.open("/models", None, "application/json", &mut watch)?;
        if !(200..300).contains(&head.status) {
            return Err(error_of(&mut conn, &head, &mut watch));
        }
        let body = conn.read_all(MAX_WHOLE_BODY.min(1024 * 1024), &mut watch)?;
        let reply: Value = serde_json::from_slice(&body).map_err(|_| ProviderError::InvalidOutput("the list of models of the service is not JSON".into()))?;
        let mut names: Vec<String> = Vec::new();
        let entries = reply.get("data").or_else(|| reply.get("models")).and_then(Value::as_array).cloned().unwrap_or_default();
        for entry in entries {
            let name = ["id", "name", "model"].iter().find_map(|k| entry.get(k).and_then(Value::as_str));
            if let Some(name) = name.filter(|n| !n.is_empty() && n.len() <= 200 && !n.chars().any(char::is_control)) {
                if names.len() < MAX_LISTED_MODELS && !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
        }
        Ok(names)
    }
}

fn info_of(provider: &str, m: &ModelSpec) -> ModelInfo {
    ModelInfo {
        model: ModelRef::new(provider, m.name.clone()),
        display_name: m.name.clone(),
        family: m.family.as_deref().map(Family::from_filename).unwrap_or_else(|| Family::from_filename(&m.name)),
        caps: Capabilities {
            tools: m.shape.tools,
            structured: m.shape.structured,
            reasoning: if m.reasoning { ReasoningSupport::Toggle } else { ReasoningSupport::None },
            vision: false,
            logprobs: false,
            seed: m.shape.seed,
            prompt_cache: PromptCache::None,
        },
        limits: Limits { context: m.context, max_output: m.max_output },
        // The server is another process on this machine: it has no size here that Aqua could say.
        residency: Residency::Local { bytes: 0, quant: None },
    }
}

impl ModelProvider for OpenAiCompatProvider {
    fn id(&self) -> &str {
        &self.spec.id
    }

    fn egress(&self) -> Egress {
        self.spec.endpoint.egress()
    }

    fn list(&self) -> Vec<ModelInfo> {
        self.spec.models.iter().map(|m| info_of(&self.spec.id, m)).collect()
    }

    fn chat(&self, model: &str, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        if let Some(why) = self.spec.endpoint.refusal() {
            return Err(ProviderError::Unavailable { detail: why });
        }
        let Some(listed) = self.model(model) else {
            return Err(ProviderError::Rejected(format!("the model {model} is not listed under {} in providers.json, and only the models listed there are asked", self.spec.id)));
        };
        let body = chat_body(model, req, &listed.shape)?;
        let payload = body.to_string();
        if payload.len() > MAX_REQUEST_BYTES {
            return Err(ProviderError::Rejected(format!("the request is {} KiB, more than the {} KiB that is sent to a model service", payload.len() / 1024, MAX_REQUEST_BYTES / 1024)));
        }
        let deadline = ctl.deadline.min(self.spec.timeout);
        let streaming = req.stream;
        let (idle, first_event) = if streaming { watch_limits(ctl) } else { (None, None) };
        let mut watch = Watch::new(ctl, deadline, streaming);
        watch.idle = idle;
        watch.first_event = first_event;
        let accept = if streaming { "text/event-stream" } else { "application/json" };
        let (mut conn, head) = self.open("/chat/completions", Some(&payload), accept, &mut watch)?;
        if !(200..300).contains(&head.status) {
            return Err(error_of(&mut conn, &head, &mut watch));
        }
        let outcome_model = ModelRef::new(self.spec.id.clone(), model);
        let mut reader = OpenAiReader::new(sink);
        let sse = head.header("content-type").map(|t| t.to_ascii_lowercase().contains("text/event-stream")).unwrap_or(streaming);
        if sse {
            let mut splitter = EventSplitter::default();
            let mut total = 0usize;
            'read: while let Some(piece) = conn.next_piece(&mut watch)? {
                total += piece.len();
                if total > MAX_STREAM_BYTES {
                    return Err(ProviderError::InvalidOutput(format!("the answer of the model service is longer than {} MiB", MAX_STREAM_BYTES / (1024 * 1024))));
                }
                for data in splitter.feed(&piece) {
                    // Every event is a sign of life, thinking as much as an answer.
                    watch.beat();
                    reader.event(&data)?;
                    if reader.done {
                        break 'read;
                    }
                }
                if splitter.pending_len() > MAX_EVENT_BYTES {
                    return Err(ProviderError::InvalidOutput("an event of the model service has no end".into()));
                }
            }
            // A stream with neither its end nor a finish reason is a service that stopped in the middle of the answer: some of it has been handed over, so this is final.
            if !reader.done && reader.finish.is_none() {
                return Err(ProviderError::Transport(format!("the model service stopped in the middle of the answer ({} characters had come)", reader.text.chars().count())));
            }
        } else {
            let bytes = conn.read_all(MAX_WHOLE_BODY, &mut watch)?;
            if conn.truncated() {
                return Err(ProviderError::Transport("the model service stopped in the middle of the answer".into()));
            }
            reader.whole(&bytes)?;
        }
        reader.finish_with(&outcome_model)
    }

    fn health(&self) -> Health {
        Health::Unknown
    }
}

/// The silence limits of a streamed call: what the call was given, else `AQUA_STREAM_IDLE_SECS` and `AQUA_STREAM_FIRST_EVENT_SECS` (off unless they are above zero).
fn watch_limits(ctl: &CallControl<'_>) -> (Option<Duration>, Option<Duration>) {
    let setting = |flag: &crate::flags::Flag| Some(crate::flags::int(flag)).filter(|secs| *secs > 0).map(|secs| Duration::from_secs(secs as u64));
    (ctl.idle.or_else(|| setting(&crate::flags::STREAM_IDLE_SECS)), ctl.first_event.or_else(|| setting(&crate::flags::STREAM_FIRST_EVENT_SECS)))
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// What watches a call while it waits: the person's Stop, the call's own flag, the time it may take, and the silence of a stream (armed only after the first event, as the local
/// server's watch is, so a prompt that takes a while to read is not taken for a stall).
struct Watch<'a, 'b> {
    ctl: &'a mut CallControl<'b>,
    began: Instant,
    deadline: Duration,
    streaming: bool,
    idle: Option<Duration>,
    first_event: Option<Duration>,
    last_event: Option<Instant>,
    last_tick: u64,
}

impl<'a, 'b> Watch<'a, 'b> {
    fn new(ctl: &'a mut CallControl<'b>, deadline: Duration, streaming: bool) -> Self {
        Self { ctl, began: Instant::now(), deadline, streaming, idle: None, first_event: None, last_event: None, last_tick: 0 }
    }

    /// An event has come.
    fn beat(&mut self) {
        self.last_event = Some(Instant::now());
    }

    /// Is the call to end now? Called in every slice of every wait.
    fn check(&mut self) -> Result<(), ProviderError> {
        if self.ctl.cancelled() || crate::modules::browser_agent::turn_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let since = self.began.elapsed();
        if since >= self.deadline {
            return Err(ProviderError::Timeout { idle: false, detail: format!("timed out waiting for the reply of the model service ({} s)", self.deadline.as_secs()) });
        }
        if self.streaming {
            let verdict = match self.last_event {
                None => self.first_event.filter(|limit| since >= *limit).map(|_| (true, since)),
                Some(at) => self.idle.filter(|limit| at.elapsed() >= *limit).map(|_| (false, at.elapsed())),
            };
            if let Some((before_first, quiet)) = verdict {
                return Err(ProviderError::Timeout { idle: true, detail: quiet_message(before_first, quiet.as_secs()) });
            }
        }
        let secs = since.as_secs();
        if secs > self.last_tick {
            self.last_tick = secs;
            self.ctl.tick(secs);
        }
        Ok(())
    }
}

/// What a stream that went quiet says. It begins as the local server's message does, so that the code that reads the text of an error finds this one too (`is_idle_text`).
fn quiet_message(before_first: bool, quiet_secs: u64) -> String {
    format!(
        "the language model went quiet {}: nothing came for {quiet_secs}s ({}). The request was hung up on; the model service is not asked to stop what it was doing.",
        if before_first { "before it began to answer" } else { "in the middle of the answer" },
        if before_first { "AQUA_STREAM_FIRST_EVENT_SECS" } else { "AQUA_STREAM_IDLE_SECS" },
    )
}

fn connect(ep: &Endpoint, watch: &mut Watch<'_, '_>) -> Result<TcpStream, ProviderError> {
    watch.check()?;
    let mut last: Option<std::io::ErrorKind> = None;
    for addr in ep.addresses()? {
        // Whatever the name led to, only this machine is connected to.
        if !addr.ip().is_loopback() {
            continue;
        }
        let limit = Duration::from_secs(CONNECT_SECS).min(watch.deadline.saturating_sub(watch.began.elapsed()).max(Duration::from_millis(50)));
        match TcpStream::connect_timeout(&addr, limit) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(e) => last = Some(e.kind()),
        }
    }
    Err(ProviderError::Unavailable {
        detail: match last {
            Some(std::io::ErrorKind::ConnectionRefused) | None => format!("nothing answers at {} (is the model server running?)", ep.authority()),
            Some(kind) => format!("could not connect to {} ({kind})", ep.authority()),
        },
    })
}

/// The status and the headers of an answer.
struct Head {
    status: u16,
    /// Names in lower case.
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// An open connection with an answer coming back on it.
struct Conn {
    stream: TcpStream,
    chunks: Option<ChunkedDecoder>,
    length: Option<usize>,
    received: usize,
    /// Bytes of the body that came with the head.
    queued: Vec<u8>,
    ended: bool,
    cut: bool,
}

impl Conn {
    fn new(stream: TcpStream) -> Self {
        Conn { stream, chunks: None, length: None, received: 0, queued: Vec::new(), ended: false, cut: false }
    }

    /// Reads until the end of the head, and sets up the reading of the body.
    fn read_head(&mut self, watch: &mut Watch<'_, '_>) -> Result<Head, ProviderError> {
        let mut raw: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        let end = loop {
            if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break at;
            }
            if raw.len() > MAX_HEAD_BYTES {
                return Err(ProviderError::InvalidOutput("the head of the answer of the model service is longer than 64 KiB".into()));
            }
            watch.check()?;
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(ProviderError::Transport("the model service closed the connection before it answered".into())),
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                Err(e) => return Err(ProviderError::Transport(format!("reading the answer failed: {}", e.kind()))),
            }
        };
        let text = String::from_utf8_lossy(&raw[..end]).into_owned();
        let mut lines = text.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let status = status_line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).filter(|c| (100..=599).contains(c));
        let Some(status) = status.filter(|_| status_line.starts_with("HTTP/")) else {
            return Err(ProviderError::InvalidOutput("the model service did not answer in HTTP".into()));
        };
        let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
        let head = Head { status, headers };
        if head.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
            self.chunks = Some(ChunkedDecoder::default());
        } else {
            self.length = head.header("content-length").and_then(|v| v.parse::<usize>().ok());
        }
        let first = &raw[end + 4..];
        if !first.is_empty() {
            self.queued = self.decode(first);
        }
        self.settle();
        Ok(head)
    }

    fn decode(&mut self, bytes: &[u8]) -> Vec<u8> {
        let out = match self.chunks.as_mut() {
            Some(decoder) => decoder.feed(bytes),
            None => bytes.to_vec(),
        };
        self.received += out.len();
        out
    }

    /// Has the body come to its end by its own framing?
    fn settle(&mut self) {
        if self.chunks.as_ref().is_some_and(ChunkedDecoder::finished) || self.length.is_some_and(|n| self.received >= n) {
            self.ended = true;
        }
    }

    /// The next piece of the body, as it comes; `None` at its end.
    fn next_piece(&mut self, watch: &mut Watch<'_, '_>) -> Result<Option<Vec<u8>>, ProviderError> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            if !self.queued.is_empty() {
                return Ok(Some(std::mem::take(&mut self.queued)));
            }
            if self.ended {
                return Ok(None);
            }
            watch.check()?;
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.ended = true;
                    // The connection ended before the framing said the body did.
                    self.cut = self.chunks.as_ref().is_some_and(|d| !d.finished()) || self.length.is_some_and(|n| self.received < n);
                    return Ok(None);
                }
                Ok(n) => {
                    self.queued = self.decode(&buf[..n]);
                    self.settle();
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                Err(e) => return Err(ProviderError::Transport(format!("reading the answer failed: {}", e.kind()))),
            }
        }
    }

    /// The body, up to `most` bytes (more is an error), read to its end.
    fn read_all(&mut self, most: usize, watch: &mut Watch<'_, '_>) -> Result<Vec<u8>, ProviderError> {
        let mut out = Vec::new();
        while let Some(piece) = self.next_piece(watch)? {
            if out.len() + piece.len() > most {
                return Err(ProviderError::InvalidOutput(format!("the answer of the model service is longer than {} KiB", most / 1024)));
            }
            out.extend_from_slice(&piece);
        }
        Ok(out)
    }

    /// Did the connection end before the body was whole?
    fn truncated(&self) -> bool {
        self.cut
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// The error of an answer that is not a success: a class by its status, its headers and the words of its body (read, never repeated; `classify_http`). A redirect is not followed and
/// not quoted.
fn error_of(conn: &mut Conn, head: &Head, watch: &mut Watch<'_, '_>) -> ProviderError {
    if (300..400).contains(&head.status) {
        return ProviderError::Rejected(format!("the model service answered with a redirect (HTTP {}), which Aqua does not follow: put the address it redirects to in providers.json", head.status));
    }
    // What can be read of the body in the time there is; an error that cannot be read is classed by its status alone.
    let mut body: Vec<u8> = Vec::new();
    while body.len() < MAX_ERROR_BODY {
        match conn.next_piece(watch) {
            Ok(Some(piece)) => body.extend_from_slice(&piece),
            _ => break,
        }
    }
    body.truncate(MAX_ERROR_BODY);
    classify_http(head.status, &head.headers, &String::from_utf8_lossy(&body), SystemTime::now())
}

/// `127.0.0.1` for a test that wants an endpoint.
pub fn loopback_endpoint(port: u16, path: &str) -> Endpoint {
    Endpoint { host: Ipv4Addr::LOCALHOST.to_string(), port, path: path.trim_end_matches('/').to_string(), reach: Reach::Loopback, tls: false }
}

/// The shape of a model when the file says nothing: plain text, no tools, no schema.
pub fn plain_shape() -> Shape {
    Shape { tools: ToolMode::None, structured: StructuredMode::None, extra_sampling: false, seed: true }
}
