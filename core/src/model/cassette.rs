//! Recorded traffic of a model service, and its replay (card OA9-05, task PR-09 of the OpenCode audit).
//!
//! Aqua had no recorded answer of any model server: a change to how a reply is read (the pieces of a stream, an error body, a `Retry-After`) was held to nothing but replies a test
//! wrote for itself. A **cassette** is one JSON file in `core/tests/fixtures/provider/` with the exchanges of a service in the order they happened: for each the request (method, url,
//! headers with every credential masked, body) and the response (status, headers, and the body **as the pieces it arrived in**, as text, or as base64 when a piece cuts a
//! character in two, which is what a network read does to a degree sign). A test replays it with no network and no model.
//!
//! What is here:
//!
//! * [`Cassette`] and its parts: the format, [`Cassette::load`], [`Cassette::to_json`] and [`Cassette::problems`], which says what is wrong with a cassette before it is committed (a
//!   credential header that is not masked, anything the secret list of `aqua_tool_host` finds, a path into a person's profile folder, an e-mail address).
//! * [`Exchange`]: what a recorder holds after one call, and [`Exchange::into_interaction`], which masks it (the credential headers, the secrets it is given and the ones the secret
//!   list knows, the person's folders) and cuts the body into pieces again. `scripts/record_provider_cassette.py` does the same in Python for the live recording, by hand and never in a
//!   test; the checks here are what it must pass.
//! * [`Replay`]: an ordered cursor over the interactions (the n-th request gets the n-th answer, as `http-recorder` of OpenCode does), and [`ReplayServer`], which serves them on a
//!   loopback port as the real HTTP of a model server: the **same reading code the engine uses for the local server** (`llama_server::post_stream` and `post_json`: the chunked
//!   decoder, the event splitter, the finish reason, the timings, the stream that ends too soon) can then be run against recorded bytes. [`engine_stream`], [`engine_json`] and
//!   [`engine_error`] do that.
//! * [`CassetteProvider`]: a [`ModelProvider`] that answers from a cassette the way a service on a network answers (an error status becomes a class through
//!   [`classify_http`] with the headers that came with it, a stream is cut into events by the engine's own `EventSplitter` and read as OpenAI-style deltas: text, thinking, tool
//!   calls assembled by `index`, `usage`), so that [`ModelRouter`](super::ModelRouter) applies its retry policy, its shorter turn after an overflow and its rules about a stream that
//!   has begun, to recorded traffic. The reading half (`OpenAiReader`) moved to `openai_wire.rs` with card OA9-04, which wrote the provider that uses it on a real connection (`openai_compat.rs`); this one reads the same events from a cassette, with no socket.
//!
//! **What this is not.** The cassettes in the folder were written by hand in the documented wire formats of llama-server, Ollama and the OpenAI API (they are not captures of a
//! running server: the run that made them had no server, no key and no network); each says so in its `note`. `scripts/record_provider_cassette.py` records a real exchange, by hand
//! (`RECORD=1`), and refuses to overwrite a file. Only compiled for tests (`cfg(test)`, and the `test-support` feature for the integration tests).

use super::error::{classify_http, ProviderError};
use super::types::{CallControl, ChatEvent, ChatOutcome, ChatRequest, Flow, ModelRef};
use super::{Capabilities, Egress, Family, Health, Limits, ModelInfo, ModelProvider, PromptCache, ReasoningSupport, Residency, StructuredMode, ToolMode};
use super::openai_wire::OpenAiReader;
use super::sse::EventSplitter;
use crate::modules::llm_router::llama_server::{self, Failure};
use aqua_tool_host::{secret_patterns, Redactor};
use base64::Engine as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// The number of the format of the cassette file. A reader refuses another.
pub const FORMAT: u32 = 1;

/// What a masked credential is written as: the text the redactor of the tool host writes.
pub const MASK: &str = "[REDACTED]";

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The format
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// The exchanges of one service with one client, in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cassette {
    pub format: u32,
    /// Who answered: `llama.cpp`, `ollama`, `openai-compat`. Names the provider of the [`CassetteProvider`] made from it.
    pub provider: String,
    /// How the service is spoken to. Only `openai-chat` (`/v1/chat/completions`, with or without `stream`) is read today.
    pub protocol: String,
    /// What the file is: where its contents came from and what it shows. A hand-written cassette says so here.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub interactions: Vec<Interaction>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    pub request: RecordedRequest,
    pub response: RecordedResponse,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    /// Names in lower case; the value of every credential header is [`MASK`].
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The JSON the client sent (a prompt of a synthetic workspace), or a string when it was not JSON.
    #[serde(default)]
    pub body: Value,
}

/// How the pieces of a response body are written in the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyEncoding {
    /// Text, one string a piece.
    #[default]
    Utf8,
    /// Bytes, one base64 string a piece: for a piece that ends inside a character.
    Base64,
}

/// A body as one string, or as the pieces it arrived in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Pieces {
    Whole(String),
    Parts(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body_encoding: BodyEncoding,
    pub body: Pieces,
    /// The connection ended before the service had finished (a server that was killed in the middle of an answer): the replay does not write the end of the body.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl RecordedResponse {
    /// The pieces of the body as bytes, in order.
    pub fn pieces(&self) -> Result<Vec<Vec<u8>>, String> {
        let parts: Vec<&str> = match &self.body {
            Pieces::Whole(s) => vec![s.as_str()],
            Pieces::Parts(p) => p.iter().map(String::as_str).collect(),
        };
        parts
            .into_iter()
            .enumerate()
            .map(|(i, part)| match self.body_encoding {
                BodyEncoding::Utf8 => Ok(part.as_bytes().to_vec()),
                BodyEncoding::Base64 => base64::engine::general_purpose::STANDARD.decode(part).map_err(|e| format!("piece {} of the body is not base64: {e}", i + 1)),
            })
            .collect()
    }

    /// The body as text (a piece that cuts a character is joined to the next one first).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.pieces().unwrap_or_default().concat()).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The headers as the pairs `classify_http` reads.
    pub fn header_list(&self) -> Vec<(String, String)> {
        self.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Is the body a stream of server-sent events?
    pub fn is_event_stream(&self) -> bool {
        self.header("content-type").is_some_and(|t| t.to_ascii_lowercase().contains("text/event-stream"))
    }
}

impl RecordedRequest {
    /// The path of the url, without the query.
    pub fn path(&self) -> String {
        path_of(&self.url)
    }

    /// Did the client ask for a stream (`"stream": true` in its body)?
    pub fn streams(&self) -> bool {
        self.body.get("stream").and_then(Value::as_bool).unwrap_or(false)
    }
}

/// `/v1/chat/completions` from `http://127.0.0.1:8080/v1/chat/completions?x=1`.
fn path_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.find('/').map_or("/", |at| &rest[at..]);
    path.split(['?', '#']).next().unwrap_or("/").to_string()
}

/// The folder the cassettes are in.
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("provider")
}

impl Cassette {
    /// A cassette from its text. Refuses a file of another format.
    pub fn parse(text: &str) -> Result<Cassette, String> {
        let cassette: Cassette = serde_json::from_str(text).map_err(|e| format!("not a cassette: {e}"))?;
        if cassette.format != FORMAT {
            return Err(format!("the cassette is of format {}, this reader reads format {FORMAT}", cassette.format));
        }
        Ok(cassette)
    }

    /// The file of the cassette called `name` in [`fixtures_dir`].
    pub fn path_of(name: &str) -> PathBuf {
        fixtures_dir().join(format!("{name}.json"))
    }

    /// Reads the cassette called `name`. A cassette that is missing is an error that says which file (a test that needs one must not pass without it).
    pub fn load(name: &str) -> Result<Cassette, String> {
        let path = Cassette::path_of(name);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("the cassette {name} cannot be read ({}): {e}", path.display()))?;
        Cassette::parse(&text).map_err(|e| format!("the cassette {name}: {e}"))
    }

    /// The names of the cassettes in the folder, sorted.
    pub fn names() -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
            .map(|dir| dir.filter_map(Result::ok).filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(str::to_string)).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The text of the file: two spaces of indent, a newline at the end.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).unwrap_or_default();
        text.push('\n');
        text
    }

    /// What is wrong with this cassette as a file to commit. An empty list is a clean one. It names the interaction and the kind of thing, and never repeats what it found.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.format != FORMAT {
            out.push(format!("the format is {}, not {FORMAT}", self.format));
        }
        for (what, value) in [("provider", &self.provider), ("protocol", &self.protocol)] {
            if value.trim().is_empty() {
                out.push(format!("the {what} is not said"));
            }
        }
        if self.interactions.is_empty() {
            out.push("there is no interaction".to_string());
        }
        out.extend(text_problems("the note", &self.note));
        for (i, it) in self.interactions.iter().enumerate() {
            let at = format!("interaction {}", i + 1);
            if it.request.method.trim().is_empty() || !(100..=599).contains(&it.response.status) {
                out.push(format!("{at}: the method or the status is not one"));
            }
            for (side, headers) in [("request", &it.request.headers), ("response", &it.response.headers)] {
                for (name, value) in headers {
                    if is_credential_header(name) && value != MASK {
                        out.push(format!("{at}: the {side} header {} holds a credential that is not masked", name.to_ascii_lowercase()));
                    }
                    out.extend(text_problems(&format!("{at}: the {side} header {name}"), &format!("{name}: {value}")));
                }
            }
            out.extend(text_problems(&format!("{at}: the url"), &it.request.url));
            out.extend(text_problems(&format!("{at}: the request body"), &it.request.body.to_string()));
            match it.response.pieces() {
                Ok(_) => out.extend(text_problems(&format!("{at}: the response body"), &it.response.text())),
                Err(e) => out.push(format!("{at}: {e}")),
            }
        }
        out
    }
}

/// A header that carries a credential or names the person's account: its value is masked in a cassette.
pub fn is_credential_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["authorization", "cookie", "key", "token", "secret", "organization", "project", "account", "session"].iter().any(|w| n.contains(w))
}

macro_rules! re {
    ($pattern:expr) => {{
        static CELL: OnceLock<Regex> = OnceLock::new();
        CELL.get_or_init(|| Regex::new($pattern).expect("a valid regular expression"))
    }};
}

/// A path into a person's own folder: `C:\Users\<name>`, `/Users/<name>`, `/home/<name>` (either slash, any doubling of it that JSON escapes make).
fn personal_path() -> &'static Regex {
    re!(r#"(?i)(?:\b[a-z]:[\\/]{1,4}users[\\/]{1,4}|/users/|/home/)[^\\/"'\s]+"#)
}

fn email() -> &'static Regex {
    re!(r"(?i)\b[a-z0-9._%+-]+@((?:[a-z0-9-]+\.)+[a-z]{2,})\b")
}

/// What is wrong with a text that is to be committed: found by the secret list (the kind only), a text the redactor would change, a personal folder, an address that is not made up.
fn text_problems(what: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let kinds: Vec<&str> = secret_patterns::scan(text).into_iter().map(|f| f.kind).collect();
    if !kinds.is_empty() {
        out.push(format!("{what} looks like it holds a secret ({})", kinds.join(", ")));
    } else if Redactor::new().redact(text) != text {
        out.push(format!("{what} holds something the redactor would mask"));
    }
    if personal_path().is_match(text) {
        out.push(format!("{what} names a folder of a person's own (a synthetic workspace is C:\\work\\demo)"));
    }
    if email().captures_iter(text).any(|c| !c[1].to_ascii_lowercase().starts_with("example.")) {
        out.push(format!("{what} holds an e-mail address that is not at example.com"));
    }
    out
}

/// `text` with the person's own folders and addresses replaced by made-up ones. Of plain text (a header, a url, one string of a JSON body), never of text that holds JSON strings in
/// their written form (a stream of events): a backslash is doubled there, and `C:\work` put in its place would be an escape that is not one.
pub fn mask_personal(text: &str) -> String {
    let text = personal_path().replace_all(text, |c: &regex::Captures| if c[0].starts_with('/') { "/work".to_string() } else { "C:\\work".to_string() });
    email().replace_all(&text, "user@example.com").into_owned()
}

/// `value` with `clean` applied to every string in it (the keys are left as they are).
fn clean_json(value: &Value, clean: &dyn Fn(&str) -> String) -> Value {
    match value {
        Value::String(s) => Value::String(clean(s)),
        Value::Array(items) => Value::Array(items.iter().map(|v| clean_json(v, clean)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), clean_json(v, clean))).collect()),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// One exchange as a recorder saw it on the wire, before anything was masked.
#[derive(Clone, Debug, Default)]
pub struct Exchange {
    pub method: String,
    pub url: String,
    pub request_headers: Vec<(String, String)>,
    pub request_body: String,
    pub status: u16,
    pub response_headers: Vec<(String, String)>,
    /// The body as the reads of the network gave it (after the chunked framing is taken off).
    pub response_pieces: Vec<Vec<u8>>,
    pub truncated: bool,
}

impl Exchange {
    /// The interaction to write to a cassette: every credential header masked, the `secrets` (a key the recorder sent, and its encodings) and everything the secret list knows masked
    /// wherever they are, the person's folders and addresses replaced in the request (its JSON is read, and each string cleaned). A body that is not text is refused, and so is a
    /// response that names a folder of a person's own: it holds JSON strings in their written form, where a folder cannot be replaced without breaking the text, so the call is to be
    /// made again in a synthetic workspace. A response that had to be changed (a secret in it) loses the boundaries of its pieces (it is written as one piece); one that did not keeps
    /// them, as text, or as base64 when a piece ends inside a character.
    pub fn into_interaction(self, secrets: &[&str]) -> Result<Interaction, String> {
        let mut redactor = Redactor::new();
        for secret in secrets {
            redactor.add_secret(secret);
        }
        let clean = |text: &str| mask_personal(&redactor.redact(text));
        let headers = |list: Vec<(String, String)>| -> BTreeMap<String, String> {
            list.into_iter().map(|(name, value)| (name.to_ascii_lowercase(), if is_credential_header(&name) { MASK.to_string() } else { clean(&value) })).collect()
        };
        let body = match serde_json::from_str::<Value>(&self.request_body) {
            Ok(json) => clean_json(&json, &clean),
            Err(_) => Value::String(clean(&self.request_body)),
        };
        let request = RecordedRequest { method: self.method.to_ascii_uppercase(), url: clean(&self.url), headers: headers(self.request_headers), body };
        let whole = String::from_utf8(self.response_pieces.concat()).map_err(|_| "the response body is not text".to_string())?;
        let cleaned = email().replace_all(&redactor.redact(&whole), "user@example.com").into_owned();
        if personal_path().is_match(&cleaned) {
            return Err("the response body names a folder of a person's own, which cannot be replaced inside the text of a stream: make the call again in a synthetic workspace".to_string());
        }
        let (body_encoding, body) = if cleaned != whole {
            (BodyEncoding::Utf8, Pieces::Parts(vec![cleaned]))
        } else if self.response_pieces.iter().all(|p| std::str::from_utf8(p).is_ok()) {
            (BodyEncoding::Utf8, Pieces::Parts(self.response_pieces.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect()))
        } else {
            (BodyEncoding::Base64, Pieces::Parts(self.response_pieces.iter().map(|p| base64::engine::general_purpose::STANDARD.encode(p)).collect()))
        };
        let response = RecordedResponse { status: self.status, headers: headers(self.response_headers), body_encoding, body, truncated: self.truncated };
        Ok(Interaction { request, response })
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// The interactions of a cassette in order: the first request gets the first answer, the second the second. A request that is not the one the next interaction recorded (another method,
/// another path, a stream where a plain answer was recorded) is not answered and does not move the cursor.
pub struct Replay {
    cassette: Cassette,
    cursor: AtomicUsize,
}

impl Replay {
    pub fn new(cassette: Cassette) -> Replay {
        Replay { cassette, cursor: AtomicUsize::new(0) }
    }

    /// The replay of the cassette `name`. Panics when the file is missing or unreadable: a test that needs a cassette fails without it, and says which.
    pub fn load(name: &str) -> Replay {
        Replay::new(Cassette::load(name).unwrap_or_else(|e| panic!("{e}")))
    }

    pub fn cassette(&self) -> &Cassette {
        &self.cassette
    }

    /// How many interactions have been answered.
    pub fn used(&self) -> usize {
        self.cursor.load(Ordering::SeqCst)
    }

    /// How many are left.
    pub fn left(&self) -> usize {
        self.cassette.interactions.len().saturating_sub(self.used())
    }

    /// The next interaction, when the request is the one it recorded: `method` and, when given, `path`, and, when given, whether the request asks for a stream.
    pub fn next(&self, method: &str, path: Option<&str>, stream: Option<bool>) -> Result<&Interaction, String> {
        let at = self.cursor.load(Ordering::SeqCst);
        let Some(interaction) = self.cassette.interactions.get(at) else {
            return Err(format!("the cassette has no interaction after the {at} it holds: this request was not recorded"));
        };
        let request = &interaction.request;
        if !request.method.eq_ignore_ascii_case(method) {
            return Err(format!("interaction {} recorded a {} and the request is a {method}", at + 1, request.method));
        }
        if let Some(path) = path.filter(|p| *p != request.path()) {
            return Err(format!("interaction {} recorded the path {} and the request is for {path}", at + 1, request.path()));
        }
        if let Some(asked) = stream.filter(|s| *s != request.streams()) {
            return Err(format!("interaction {} recorded a call {} a stream and the request {} one", at + 1, if request.streams() { "for" } else { "without" }, if asked { "asks for" } else { "does not ask for" }));
        }
        self.cursor.fetch_add(1, Ordering::SeqCst);
        Ok(interaction)
    }
}

/// What the loopback server was asked, for a test to look at.
#[derive(Clone, Debug, PartialEq)]
pub struct SeenRequest {
    pub method: String,
    pub path: String,
    pub body: Value,
}

struct ServerState {
    replay: Replay,
    seen: Mutex<Vec<SeenRequest>>,
    problems: Mutex<Vec<String>>,
    /// The wait between two pieces of a body, so that the reads of the client are cut at the pieces (the network may still join some).
    pace: Duration,
}

/// A cassette served on a port of the loopback address, one connection at a time, as the HTTP of a model server (`Connection: close`, chunked when the recording was). It listens on
/// `127.0.0.1` and on a port the system chooses, never on one of Aqua's. Stopped when it is dropped.
pub struct ReplayServer {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    state: Arc<ServerState>,
}

impl ReplayServer {
    pub fn start(cassette: Cassette) -> std::io::Result<ReplayServer> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let state = Arc::new(ServerState { replay: Replay::new(cassette), seen: Mutex::new(Vec::new()), problems: Mutex::new(Vec::new()), pace: Duration::from_millis(3) });
        let stop = Arc::new(AtomicBool::new(false));
        let (state_for_thread, stop_for_thread) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        serve_connection(stream, &state_for_thread);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(2)),
                    Err(_) => break,
                }
            }
        });
        Ok(ReplayServer { port, stop, thread: Some(thread), state })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The requests it was given, in order.
    pub fn requests(&self) -> Vec<SeenRequest> {
        self.state.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Requests it could not answer from the cassette (one that was not recorded, or came out of order). A test ends by asking for none.
    pub fn problems(&self) -> Vec<String> {
        self.state.problems.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Interactions not answered yet.
    pub fn left(&self) -> usize {
        self.state.replay.left()
    }
}

impl Drop for ReplayServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The method, the path and the body of the request on `stream`, read in full. `None` when the client went away or sent something that is not HTTP.
fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if raw.len() > 64 * 1024 {
            return None;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let (method, target) = (first.next()?.to_string(), first.next()?.to_string());
    let length = lines.filter_map(|l| l.split_once(':')).find(|(n, _)| n.trim().eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse::<usize>().ok()).unwrap_or(0);
    let mut body: Vec<u8> = raw[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => body.extend_from_slice(&buf[..n]),
        }
    }
    Some((method, path_of(&target), body))
}

fn serve_connection(mut stream: TcpStream, state: &ServerState) {
    let Some((method, path, body)) = read_request(&mut stream) else { return };
    let body_json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.seen.lock().unwrap_or_else(|e| e.into_inner()).push(SeenRequest { method: method.clone(), path: path.clone(), body: body_json.clone() });
    let asked_stream = body_json.get("stream").and_then(Value::as_bool).unwrap_or(false);
    match state.replay.next(&method, Some(&path), Some(asked_stream)) {
        Ok(interaction) => {
            let _ = write_response(&mut stream, &interaction.response, state.pace);
        }
        Err(problem) => {
            state.problems.lock().unwrap_or_else(|e| e.into_inner()).push(problem);
            let text = "{\"error\":{\"message\":\"no recorded answer for this request\",\"type\":\"cassette_miss\"}}";
            let _ = write!(stream, "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
        }
    }
    let _ = stream.flush();
    // The end of the body is the end of the connection (`Connection: close`): a FIN after what was written, which the client reads to the end before it sees it.
    let _ = stream.shutdown(Shutdown::Write);
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Writes a recorded response as HTTP/1.1. The framing is the server's and not the cassette's to say twice: chunked when the recording was (or was a stream), else a length.
fn write_response(stream: &mut TcpStream, response: &RecordedResponse, pace: Duration) -> std::io::Result<()> {
    let pieces: Vec<Vec<u8>> = response.pieces().map_err(std::io::Error::other)?.into_iter().filter(|p| !p.is_empty()).collect();
    let chunked = response.is_event_stream() || response.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason(response.status));
    for (name, value) in &response.headers {
        if !["content-length", "transfer-encoding", "connection"].iter().any(|skip| name.eq_ignore_ascii_case(skip)) {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    } else {
        head.push_str(&format!("Content-Length: {}\r\n", pieces.iter().map(Vec::len).sum::<usize>()));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    for piece in &pieces {
        if chunked {
            write!(stream, "{:x}\r\n", piece.len())?;
            stream.write_all(piece)?;
            stream.write_all(b"\r\n")?;
        } else {
            stream.write_all(piece)?;
        }
        stream.flush()?;
        std::thread::sleep(pace);
    }
    if chunked && !response.truncated {
        stream.write_all(b"0\r\n\r\n")?;
    }
    stream.flush()
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The engine's own reading of the local server's answer, run against a replay
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// What the engine's reading of a streamed answer gave: the pieces of text it handed over as they came, and the reply it made of the whole.
#[derive(Debug)]
pub struct EngineRead {
    pub pieces: Vec<String>,
    pub result: Result<Value, String>,
}

/// A request for a streamed answer to the replay on `port`, read by `llama_server::post_stream`: the code the engine reads the local server with (the chunked decoder, the event
/// splitter, the finish reason and the timings, the end of a stream that came too soon).
pub fn engine_stream(port: u16, body: &Value) -> EngineRead {
    let abort = AtomicBool::new(false);
    let mut pieces = Vec::new();
    let result = llama_server::post_stream(port, "replay-key", "/v1/chat/completions", body, Duration::from_secs(20), &abort, |piece| pieces.push(piece.to_string()));
    EngineRead { pieces, result }
}

/// The same for a plain answer (`llama_server::post_json`).
pub fn engine_json(port: u16, body: &Value) -> Result<Value, String> {
    llama_server::post_json(port, "replay-key", "/v1/chat/completions", body, Duration::from_secs(20), &AtomicBool::new(false))
}

/// The class the engine gives the error text of a request (`HTTP 400: ...`, `the model server stopped ...`): a refusal of the one request (an HTTP 4xx) is `Failure::Rejected` with the
/// sentence `refusal_message` writes, anything else is a server that cannot be used; then the conversion the model layer makes.
pub fn engine_error(error: &str) -> ProviderError {
    let failure = if llama_server::is_client_rejection(error) { Failure::Rejected(llama_server::refusal_message(error)) } else { Failure::Unavailable(error.to_string()) };
    ProviderError::from(failure)
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// A provider that answers from a cassette
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// A [`ModelProvider`] whose answers are the responses of a cassette, in order. It is `Loopback` (the router will ask it), and every call takes the next interaction: a call whose
/// stream flag is not the one recorded is refused with `Unavailable` naming the difference.
pub struct CassetteProvider {
    id: String,
    model: String,
    egress: Egress,
    replay: Replay,
}

impl CassetteProvider {
    /// A provider called as the cassette's `provider`, with one model, `recorded`.
    pub fn new(cassette: Cassette) -> CassetteProvider {
        CassetteProvider { id: cassette.provider.clone(), model: "recorded".into(), egress: Egress::Loopback, replay: Replay::new(cassette) }
    }

    pub fn load(name: &str) -> CassetteProvider {
        CassetteProvider::new(Cassette::load(name).unwrap_or_else(|e| panic!("{e}")))
    }

    pub fn with_egress(mut self, egress: Egress) -> Self {
        self.egress = egress;
        self
    }

    /// The model reference a call to this provider is made on.
    pub fn model_ref(&self) -> ModelRef {
        ModelRef::new(self.id.clone(), self.model.clone())
    }

    /// Interactions not used yet.
    pub fn left(&self) -> usize {
        self.replay.left()
    }

    /// Interactions used.
    pub fn used(&self) -> usize {
        self.replay.used()
    }
}

impl ModelProvider for CassetteProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn egress(&self) -> Egress {
        self.egress.clone()
    }

    fn list(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            model: self.model_ref(),
            display_name: self.model.clone(),
            family: Family::Other(self.id.clone()),
            caps: Capabilities { tools: ToolMode::Native, structured: StructuredMode::JsonObject, reasoning: ReasoningSupport::None, vision: false, logprobs: false, seed: true, prompt_cache: PromptCache::None },
            limits: Limits { context: 8192, max_output: 1200 },
            residency: Residency::Local { bytes: 0, quant: None },
        }]
    }

    fn chat(&self, model: &str, req: &ChatRequest<'_>, ctl: &mut CallControl<'_>, sink: &mut dyn FnMut(ChatEvent) -> Flow) -> Result<ChatOutcome, ProviderError> {
        let interaction = self.replay.next("POST", None, Some(req.stream)).map_err(|detail| ProviderError::Unavailable { detail })?;
        if ctl.cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let response = &interaction.response;
        // An error reply is a class by its status, its headers (Retry-After) and the words of its body, which are read and not repeated.
        if !(200..300).contains(&response.status) {
            return Err(classify_http(response.status, &response.header_list(), &response.text(), SystemTime::now()));
        }
        let pieces = response.pieces().map_err(ProviderError::InvalidOutput)?;
        let mut reader = OpenAiReader::new(sink);
        if response.is_event_stream() {
            let mut splitter = EventSplitter::default();
            'pieces: for piece in &pieces {
                for data in splitter.feed(piece) {
                    reader.event(&data)?;
                    if reader.done {
                        break 'pieces;
                    }
                }
                if ctl.cancelled() {
                    return Err(ProviderError::Cancelled);
                }
            }
            // A stream with neither its end nor a finish reason is a service that stopped in the middle of the answer: some of it has been handed over, so this is final.
            if !reader.done && reader.finish.is_none() {
                return Err(ProviderError::Transport(format!("the model service stopped in the middle of the answer ({} characters had come)", reader.text.chars().count())));
            }
        } else {
            reader.whole(&pieces.concat())?;
        }
        reader.finish_with(&ModelRef::new(self.id.clone(), model))
    }

    fn health(&self) -> Health {
        Health::Ready
    }
}
