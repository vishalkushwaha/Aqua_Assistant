//! `aqua-cli acp`: the Agent Client Protocol over standard input and output, so that an editor (Zed, JetBrains, Neovim) can drive Aqua (card OA10-07; task UX-12 of the OpenCode audit;
//! decisions D-09 and D-06 taken as the audit recommends). The page that describes it for a person is `documentation/ACP.md`.
//!
//! ```text
//! aqua-cli acp [--core URL] [--data-dir DIR] [--token-file FILE] [--wait-approval SECS] [--profile coding|files|web|desktop]
//! ```
//!
//! **What it is.** A client of the core's HTTP API, like `aqua-cli run`, that speaks JSON-RPC 2.0 to an editor: one message a line on standard input and standard output, no `Content-Length`
//! header, no embedded line break (the protocol's stdio transport). It is started by the editor, never by Aqua, and does nothing until an editor sends `initialize`. It is written by hand
//! over `serde_json` (decision D-06: no crate for the protocol): the subset below is all of it.
//!
//! | Editor sends | What happens |
//! |---|---|
//! | `initialize` | answers with protocol version 1 and what this agent can do: `loadSession`, `sessionCapabilities {list, resume}`, `promptCapabilities {embeddedContext}` (no image, no audio), no `authMethods` |
//! | `session/new {cwd}` | makes a session name; **nothing is started yet** (the run is started by the first prompt) |
//! | `session/prompt {sessionId, prompt}` | `POST /api/v1/runs` with the text, the embedded context (cut to 4,000 characters in all), the folder `cwd`, `profile: coding` and the session; follows the run (`run::watch`, so a broken stream is taken up again) and sends `session/update` notifications; ends with `{stopReason}` |
//! | `session/cancel {sessionId}` (notification) | asks the core to end the run; the prompt answers `cancelled` |
//! | `session/list {cwd?, cursor?}` | `GET /api/v1/sessions` (the sessions this token started) |
//! | `session/load {sessionId, cwd}` | `GET /api/v1/sessions/{id}`; the earlier runs are sent back as `user_message_chunk` and `agent_message_chunk` updates |
//! | `session/resume {sessionId, cwd}` | the same without the replay |
//!
//! **The updates.** `text` events become `agent_message_chunk`, a step `agent_thought_chunk`, `tool.started` a `tool_call` (title: the tool's name; kind from the name) and `tool.finished` a
//! `tool_call_update` (`completed` or `failed`), `usage` a `usage_update`, and a question, a limit or an error a message. **A change that waits for an approval is a tool call with the status
//! `pending` and the words "Waiting for your approval in the Aqua app"**: an approval is answered in the app with the app's token, never by this process (decision D-09: the API token
//! cannot approve, and the editor's own permission dialog, `session/request_permission`, is **not used**). There is no `file.changed` event yet (card UX-07), so a tool call carries no diff.
//!
//! **What it does not do.** It reads no file and writes no file: it never calls the editor's `fs/*` or `terminal/*` methods (it sends no request to the editor at all) and opens nothing
//! itself; `cwd` is passed to the core, which refuses a folder Aqua may not use. It starts no MCP server an editor names (`mcpServers` is ignored; Aqua has its own tool host). It never
//! writes the API token (or what any file of the data folder holds) to the stream; the stream carries only protocol frames, and what this process has to say about itself goes to the error stream.
//!
//! **Limits.** A message from the editor is at most 1 MiB (a longer line is refused with `limit_reached` and skipped to its end, and the process goes on); a text sent to the editor is at
//! most 64 KiB; at most 64 sessions are kept in memory (the oldest idle one is dropped: `session/load` brings it back from the core's record) and at most 8 prompts go at once (the core
//! allows 8 runs). When the editor closes the input, every run that this process started and that is still going is asked to end.
//!
//! **Errors** are JSON-RPC errors: the numeric `code` is the protocol's (`-32700` parse error, `-32600` invalid request, `-32601` method not found, `-32602` invalid params, `-32603`
//! internal, `-32000` authentication needed, `-32002` not found) and `error.data.code` is one of the stable codes of the API (`documentation/ERROR_CODES.md`: `invalid_request`,
//! `not_found`, `unauthorized`, `forbidden_scope`, `limit_reached`, `not_ready`, `conflict`, ...), with `status` when the core said it.

use super::cmds::encode;
use super::http::{core_address, read_token, safe_id, Client, Frame, NetError, TokenKind};
use super::run::{resolve_outcome, watch, Outcome, Stop, Watch, Watcher, Wish, EXIT_CANCELLED, EXIT_LIMIT};
use super::view::safe;
use super::{parse, COMMON};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The version of the protocol that this agent speaks.
pub const PROTOCOL_VERSION: u64 = 1;
/// The longest message from the editor (a line), in bytes.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// The longest text sent to the editor in one field, in bytes.
const MAX_TEXT_BYTES: usize = 64 * 1024;
/// How many sessions are kept in memory.
const MAX_SESSIONS: usize = 64;
/// How many prompts go at once.
const MAX_PROMPTS: usize = 8;
/// The most characters of a goal the core takes (`agent_runs::store::GOAL_RESUME_CHARS`).
const MAX_GOAL_CHARS: usize = 4_000;
/// The longest path of a folder.
const MAX_CWD_CHARS: usize = 1_024;
/// How many earlier runs `session/load` sends back.
const MAX_REPLAY_RUNS: usize = 20;
/// How many sessions `session/list` asks for.
const LIST_LIMIT: u32 = 50;
/// How long a read of the stream waits before the prompt looks at its flags: it is also how late a cancel is noticed.
const LOOK: Duration = Duration::from_millis(200);
/// How long the process waits for the runs it ended to say so, after the editor closed the input.
const CLOSE_WAIT: Duration = Duration::from_secs(3);
/// How long a question of the host is left open when nothing says otherwise: a person is at the editor, and the card is in another window.
pub const DEFAULT_WAIT_APPROVAL: u64 = 120;

/// What `acp` works with: the streams, and the unit of the wait before a broken stream is taken up again.
pub struct AcpIo {
    /// What the editor writes (standard input).
    pub input: Box<dyn BufRead + Send>,
    /// What the editor reads (standard output): protocol frames and nothing else.
    pub output: Box<dyn Write + Send>,
    /// What this process says about itself (standard error).
    pub log: Box<dyn Write + Send>,
    /// Is standard input a terminal: a person who started it by hand, not an editor.
    pub stdin_is_terminal: bool,
    pub backoff_unit: Duration,
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Errors

/// A JSON-RPC error: the protocol's number, a sentence, and `data.code`, one of the stable codes of the API.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Value,
}

impl RpcError {
    fn new(code: i64, stable: &str, message: &str) -> Self {
        Self { code, message: bounded(&clean(message), 600), data: json!({ "code": stable }) }
    }

    pub fn parse_error(message: &str) -> Self {
        Self::new(-32700, "invalid_request", message)
    }

    pub fn invalid_request(message: &str) -> Self {
        Self::new(-32600, "invalid_request", message)
    }

    pub fn invalid_params(message: &str) -> Self {
        Self::new(-32602, "invalid_request", message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, "not_found", &format!("this agent has no method called {}", bounded(&clean(method), 80)))
    }

    pub fn too_large() -> Self {
        Self::new(-32600, "limit_reached", &format!("a message is at most {MAX_MESSAGE_BYTES} bytes: that one was skipped"))
    }

    pub fn conflict(message: &str) -> Self {
        Self::new(-32600, "conflict", message)
    }

    pub fn limit(message: &str) -> Self {
        Self::new(-32600, "limit_reached", message)
    }

    pub fn not_found(message: &str) -> Self {
        Self::new(-32002, "not_found", message)
    }

    pub fn unauthorized(message: &str) -> Self {
        Self::new(-32000, "unauthorized", message)
    }

    /// An error answer of the core: its own stable code goes on in `data.code` (a word that is not one of the fifteen is `internal`, as the contract says), and its status in `data.status`.
    pub fn from_core(status: u16, message: &str, code: &str) -> Self {
        let stable = if STABLE_CODES.contains(&code) { code.to_string() } else { code_of_status(status).to_string() };
        let number = match stable.as_str() {
            "unauthorized" | "forbidden_scope" | "host_not_allowed" | "origin_not_allowed" => -32000,
            "not_found" => -32002,
            "invalid_request" => -32602,
            _ => -32603,
        };
        let mut data = json!({ "code": stable, "status": status });
        if matches!(stable.as_str(), "busy" | "not_ready" | "timeout") {
            data["retryable"] = json!(true);
        }
        Self { code: number, message: bounded(&clean(message), 600), data }
    }

    /// A request that got no answer: nothing is listening (`not_ready`, which can be tried again) or the connection broke (`internal`).
    pub fn from_net(e: &NetError) -> Self {
        match e {
            NetError::Unreachable(m) => Self { code: -32603, message: bounded(&clean(m), 600), data: json!({ "code": "not_ready", "retryable": true }) },
            NetError::Broken(m) => Self { code: -32603, message: bounded(&clean(m), 600), data: json!({ "code": "internal" }) },
        }
    }

    fn body(&self) -> Value {
        json!({ "code": self.code, "message": self.message, "data": self.data })
    }
}

/// The fifteen codes of `documentation/ERROR_CODES.md`.
const STABLE_CODES: [&str; 15] = [
    "unauthorized",
    "forbidden_scope",
    "host_not_allowed",
    "origin_not_allowed",
    "busy",
    "not_ready",
    "not_found",
    "conflict",
    "too_late",
    "limit_reached",
    "invalid_request",
    "model_unavailable",
    "timeout",
    "cancelled",
    "internal",
];

/// The code the contract gives a status when the body names none (`ErrorCode::for_status`).
fn code_of_status(status: u16) -> &'static str {
    match status {
        401 => "unauthorized",
        403 => "forbidden_scope",
        404 => "not_found",
        408 | 504 => "timeout",
        409 => "conflict",
        410 => "too_late",
        429 => "limit_reached",
        503 => "not_ready",
        400..=499 => "invalid_request",
        _ => "internal",
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Text

/// A text for the editor: line breaks stay, tabs stay as a space, every other control character is replaced, an invisible one is shown by its number.
pub fn clean(text: &str) -> String {
    text.replace("\r\n", "\n").split('\n').map(safe).collect::<Vec<_>>().join("\n")
}

/// `text` cut to at most `max` bytes at a character boundary, with a mark when something was left out.
pub fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub(16);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} ... (cut)", &text[..end])
}

/// What the editor shows for a tool: its kind, from the name of the tool.
pub fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "fs.read" | "fs.list" | "fs.stat" | "fs.outline" | "code.outline" | "code.symbols" | "doc.read" | "project.profile" | "skill.file" | "skill.load" => "read",
        "fs.grep" | "fs.find" | "git.grep" | "doc.search" | "tools.search" | "memory.search" | "web.search" | "screen.find_text" => "search",
        "fs.write" | "fs.edit" | "fs.edit_lines" | "fs.patch" | "fs.set_attrs" | "fs.restore" | "fs.undo_run" | "git.resolve_conflict" | "win.multi_edit" => "edit",
        "fs.delete" | "git.branch_delete" | "git.stash_drop" | "git.discard" | "docker.rm" | "docker.rmi" => "delete",
        "fs.move" | "fs.copy" => "move",
        "web.fetch" | "web.read" | "web.scrape" | "http.request" => "fetch",
        t if t.starts_with("term.") || t == "check.run" || t.starts_with("docker.") || t.starts_with("ssh.") || t.starts_with("iac.") || t.starts_with("git.") || t.starts_with("db.") || t.starts_with("proc.") => "execute",
        t if t.starts_with("forge.") => "fetch",
        _ => "other",
    }
}

/// A moment as `2026-10-11T08:30:00.000Z` (milliseconds since 1970).
pub fn iso8601(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Is `cwd` an absolute path as the editor may mean it (a drive letter, a UNC name or a slash), of a sane length, with no control character? Nothing is read from the disk.
fn valid_cwd(cwd: &str) -> bool {
    let n = cwd.chars().count();
    if n == 0 || n > MAX_CWD_CHARS || cwd.chars().any(|c| c.is_control()) {
        return false;
    }
    let b = cwd.as_bytes();
    cwd.starts_with('/') || cwd.starts_with("\\\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

/// A session of the core is named by 1 to 64 letters, digits and `_ . : -`; the one that goes into a path has no colon.
fn valid_session(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && safe_id(id)
}

/// The goal of a run from the blocks of a prompt: the request (the text blocks) first, then the context the editor embedded, in all at most 4,000 characters. The flag says that something
/// was left out. Images and sound are refused (the agent said it takes none), and so is a prompt with no text.
pub fn goal_from_prompt(blocks: &[Value]) -> Result<(String, bool), RpcError> {
    let mut request: Vec<String> = Vec::new();
    let mut context: Vec<String> = Vec::new();
    for block in blocks {
        match block["type"].as_str().unwrap_or("") {
            "text" => request.push(clean(block["text"].as_str().unwrap_or(""))),
            "resource" => {
                let uri = clean(block["resource"]["uri"].as_str().unwrap_or("")).chars().take(300).collect::<String>();
                match block["resource"]["text"].as_str() {
                    Some(text) => context.push(format!("[context from {uri}]\n{}", clean(text))),
                    None => context.push(format!("[context from {uri}: not text, left out]")),
                }
            }
            "resource_link" => {
                let uri = clean(block["uri"].as_str().unwrap_or("")).chars().take(300).collect::<String>();
                let name = clean(block["name"].as_str().unwrap_or("")).chars().take(120).collect::<String>();
                context.push(format!("[file: {uri}{}]", if name.is_empty() { String::new() } else { format!(" ({name})") }));
            }
            "image" | "audio" => return Err(RpcError::invalid_params("this agent takes text and embedded text only: no image and no audio")),
            other => return Err(RpcError::invalid_params(&format!("a prompt block of type {} is not one this agent takes", bounded(&clean(other), 40)))),
        }
    }
    let request = request.join("\n\n");
    if request.trim().is_empty() {
        return Err(RpcError::invalid_params("the prompt has no text"));
    }
    let mut goal: String = request.trim().chars().take(MAX_GOAL_CHARS).collect();
    let mut cut = request.trim().chars().count() > MAX_GOAL_CHARS;
    for piece in context {
        let room = MAX_GOAL_CHARS.saturating_sub(goal.chars().count() + 2);
        if room < 40 {
            cut = true;
            break;
        }
        goal.push_str("\n\n");
        if piece.chars().count() > room {
            goal.extend(piece.chars().take(room.saturating_sub(5)));
            goal.push_str(" ...");
            cut = true;
        } else {
            goal.push_str(&piece);
        }
    }
    Ok((goal, cut))
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The events of a run, as the editor's updates

/// Turns the events of one run into `session/update` payloads, one event at a time.
pub struct Translator {
    run_id: String,
    /// The calls that have not finished, oldest first (a run does one at a time).
    open: Vec<i64>,
    approval_open: bool,
    last_title: String,
}

impl Translator {
    pub fn new(run_id: &str) -> Self {
        Self { run_id: run_id.to_string(), open: Vec::new(), approval_open: false, last_title: String::new() }
    }

    fn call_id(&self, call: i64) -> String {
        format!("{}.{call}", self.run_id)
    }

    fn message(text: &str) -> Value {
        json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": bounded(&clean(text), MAX_TEXT_BYTES) } })
    }

    fn note_on(&self, call: i64, status: &str, text: &str) -> Value {
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": self.call_id(call), "status": status, "content": [{ "type": "content", "content": { "type": "text", "text": bounded(&clean(text), MAX_TEXT_BYTES) } }] })
    }

    /// The updates for one event (none for the ones that have nothing for an editor).
    pub fn updates(&mut self, ev: &Value) -> Vec<Value> {
        let s = |key: &str| ev[key].as_str().unwrap_or("").to_string();
        match ev["type"].as_str().unwrap_or("") {
            "step" => {
                let title = clean(&s("title"));
                if ev["status"].as_str() == Some("running") && !title.is_empty() && title != self.last_title {
                    self.last_title = title.clone();
                    return vec![json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": format!("{title}\n") } })];
                }
                Vec::new()
            }
            "tool.started" => {
                let call = ev["call_id"].as_i64().unwrap_or(-1);
                let tool = s("tool");
                self.open.push(call);
                if self.open.len() > 64 {
                    self.open.remove(0);
                }
                vec![json!({ "sessionUpdate": "tool_call", "toolCallId": self.call_id(call), "title": bounded(&clean(&tool), 120), "kind": tool_kind(&tool), "status": "in_progress" })]
            }
            "status" | "approval.opened" => {
                let kind = ev["type"].as_str().unwrap_or("");
                if kind == "status" && ev["state"].as_str() == Some("running") {
                    self.approval_open = false;
                    return Vec::new();
                }
                let waiting = kind == "approval.opened" || ev["state"].as_str() == Some("waiting_approval");
                if !waiting || std::mem::replace(&mut self.approval_open, true) {
                    return Vec::new();
                }
                // The question is the app's: the API token is told only that the run waits, and an approval is never answered here.
                let words = "Waiting for your approval in the Aqua app. An editor cannot approve it: open Aqua and answer there.";
                match self.open.last() {
                    Some(&call) => vec![self.note_on(call, "pending", words)],
                    None => vec![Self::message(&format!("({words})"))],
                }
            }
            "approval.closed" => {
                self.approval_open = false;
                let outcome = s("outcome");
                let words = match outcome.as_str() {
                    "timeout" => "Nobody answered in time, so the call was refused. Answer in the Aqua app sooner (aqua-cli acp --wait-approval SECS keeps a question open longer).".to_string(),
                    other => format!("Approval: {other}."),
                };
                match self.open.last() {
                    Some(&call) => vec![self.note_on(call, "in_progress", &words)],
                    None => vec![Self::message(&format!("({words})"))],
                }
            }
            "tool.finished" => {
                let call = ev["call_id"].as_i64().unwrap_or(-1);
                self.open.retain(|c| *c != call);
                let ok = ev["ok"].as_bool() != Some(false);
                let mut update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": self.call_id(call), "status": if ok { "completed" } else { "failed" } });
                let words = if !ok { format!("{} ({}).", s("status"), s("decision")) } else if !s("approval").is_empty() { format!("Approval: {}.", s("approval")) } else { String::new() };
                if !words.is_empty() {
                    update["content"] = json!([{ "type": "content", "content": { "type": "text", "text": bounded(&clean(&words), 400) } }]);
                }
                vec![update]
            }
            "usage" => match (ev["ctx_used"].as_u64(), ev["ctx_window"].as_u64()) {
                (Some(used), Some(size)) if size > 0 => vec![json!({ "sessionUpdate": "usage_update", "used": used, "size": size })],
                _ => Vec::new(),
            },
            "text" => vec![Self::message(&format!("{}\n", s("text")))],
            "limit" => vec![Self::message(&format!("Stopped by a limit: {} ({} of {}).\n", s("stopped_by"), ev["used"], ev["cap"]))],
            "question" => vec![Self::message("The agent asked a question. Answer it in the Aqua app: an editor cannot answer it.\n")],
            "error" => vec![Self::message(&format!("Error {}: {}\n", s("code"), s("message")))],
            "run.finished" => {
                let status = s("status");
                let why = s("why_it_stopped");
                match status.as_str() {
                    "completed" | "cancelled" => Vec::new(),
                    "waiting_user" => vec![Self::message("The run needs you: it asked a question. Open the Aqua app to answer.\n")],
                    _ => vec![Self::message(&format!("The run did not complete ({status}){}\n", if why.is_empty() { String::new() } else { format!(": {why}") }))],
                }
            }
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The streams

/// Where the frames go: one message a line, written whole under a lock, so that two prompts never mix a line.
#[derive(Clone)]
struct Out {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Out {
    /// One frame. `false` when the editor has gone (the write failed). A JSON text has no raw line break (they are escaped), and the two Unicode separators that some readers take for
    /// one are escaped too.
    fn send(&self, v: &Value) -> bool {
        let mut text = v.to_string().replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029");
        text.push('\n');
        let mut w = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(text.as_bytes()).and_then(|()| w.flush()).is_ok()
    }

    fn notify(&self, session: &str, update: Value) -> bool {
        self.send(&json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": session, "update": update } }))
    }

    fn respond(&self, id: &Value, result: Value) -> bool {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn fail(&self, id: &Value, e: &RpcError) -> bool {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "error": e.body() }))
    }
}

/// What the process says about itself.
#[derive(Clone)]
struct Log {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Log {
    fn line(&self, text: &str) {
        let mut w = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(w, "acp: {}", safe(text));
        let _ = w.flush();
    }
}

enum Got {
    Line,
    TooLong,
    Eof,
}

/// One line of `r` into `out` (without its line break), at most `max` bytes; a longer one is read to its end and thrown away.
fn read_bounded_line(r: &mut dyn BufRead, out: &mut Vec<u8>, max: usize) -> std::io::Result<Got> {
    let mut too_long = false;
    let mut any = false;
    loop {
        let (done, used) = {
            let available = match r.fill_buf() {
                Ok(a) => a,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if available.is_empty() {
                return Ok(if too_long {
                    Got::TooLong
                } else if any {
                    Got::Line
                } else {
                    Got::Eof
                });
            }
            any = true;
            let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
                Some(i) => (&available[..i], true),
                None => (available, false),
            };
            if !too_long {
                if out.len() + chunk.len() > max {
                    too_long = true;
                    out.clear();
                } else {
                    out.extend_from_slice(chunk);
                }
            }
            (done, chunk.len() + usize::from(done))
        };
        r.consume(used);
        if done {
            if out.last() == Some(&b'\r') {
                out.pop();
            }
            return Ok(if too_long { Got::TooLong } else { Got::Line });
        }
    }
}

enum Msg {
    Line(String),
    TooLong,
    Eof,
    Done { id: Value, session: String, result: Result<&'static str, RpcError> },
}

fn reader_thread(mut input: Box<dyn BufRead + Send>, tx: Sender<Msg>) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let msg = match read_bounded_line(&mut *input, &mut buf, MAX_MESSAGE_BYTES) {
            Ok(Got::Line) => Msg::Line(String::from_utf8_lossy(&buf).into_owned()),
            Ok(Got::TooLong) => Msg::TooLong,
            Ok(Got::Eof) | Err(_) => {
                let _ = tx.send(Msg::Eof);
                return;
            }
        };
        if tx.send(msg).is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The sessions and the prompts

/// What the process shares with the thread that follows one prompt.
#[derive(Default)]
struct PromptCtl {
    cancel: AtomicBool,
}

struct Session {
    cwd: String,
    busy: Option<Arc<PromptCtl>>,
    /// The order the sessions were made or last used in, for dropping the oldest idle one.
    tick: u64,
}

/// What a prompt's thread needs.
struct Worker {
    out: Out,
    log: Log,
    client: Client,
    wait: u64,
    profile: String,
    backoff_unit: Duration,
}

/// The watcher of a prompt: the updates of each event, and a cancel of the editor as the wish.
struct PromptWatcher<'a> {
    out: &'a Out,
    log: &'a Log,
    session: &'a str,
    ctl: &'a PromptCtl,
    translator: Translator,
    /// The editor is still reading what is written.
    alive: bool,
}

impl Watcher for PromptWatcher<'_> {
    fn frame(&mut self, frame: &Frame, event: Option<&Value>) {
        if frame.event == "end" {
            return;
        }
        if let Some(ev) = event {
            for update in self.translator.updates(ev) {
                if !self.out.notify(self.session, update) {
                    self.alive = false;
                }
            }
        }
    }

    fn wish(&mut self) -> Wish {
        if self.ctl.cancel.load(Ordering::SeqCst) || !self.alive {
            Wish::Interrupt
        } else {
            Wish::Go
        }
    }

    fn note(&mut self, text: &str) {
        self.log.line(text);
    }
}

impl Worker {
    /// One prompt: starts the run, follows it, and gives the stop reason or the error. The updates are sent as the events come.
    fn drive(&self, session: &str, cwd: &str, goal: &str, cut: bool, ctl: &PromptCtl) -> Result<&'static str, RpcError> {
        if cut {
            self.out.notify(session, Translator::message(&format!("(the request and its context were cut to {MAX_GOAL_CHARS} characters, which is the most a run takes)\n")));
        }
        let mut body = json!({ "goal": goal, "profile": self.profile, "approval_wait_secs": self.wait.clamp(10, 600), "session_id": session });
        if !cwd.is_empty() {
            body["directory"] = json!(cwd);
        }
        let reply = self.client.send("POST", "/api/v1/runs", Some(&body)).map_err(|e| RpcError::from_net(&e))?;
        if !(reply.status == 200 || reply.status == 202) {
            let (message, code) = reply.error();
            return Err(RpcError::from_core(reply.status, &message, &code));
        }
        let Some(run_id) = reply.json()["run_id"].as_str().filter(|i| safe_id(i)).map(str::to_string) else {
            return Err(RpcError::new(-32603, "internal", "the core's answer to the start of a run did not name the run"));
        };
        let cfg = Watch { stream: &self.client, control: &self.client, backoff_unit: self.backoff_unit, look_every: LOOK };
        let mut watcher = PromptWatcher { out: &self.out, log: &self.log, session, ctl, translator: Translator::new(&run_id), alive: true };
        let result = watch(&cfg, &run_id, 0, &mut watcher);
        let cancelled = ctl.cancel.load(Ordering::SeqCst);
        match result {
            Ok(mut outcome) => {
                resolve_outcome(&self.client, &run_id, &mut outcome);
                Ok(stop_reason(&outcome, cancelled))
            }
            Err(Stop::Left) => Ok("cancelled"),
            Err(Stop::Net(e)) => Err(RpcError::from_net(&e)),
            Err(Stop::Refused { status, message, code }) => Err(RpcError::from_core(status, &message, &code)),
            Err(Stop::KeepsClosing) => Err(RpcError::new(-32603, "not_ready", "the stream of the run kept closing before the run ended")),
        }
    }
}

/// The protocol's word for how a run ended: a stop the editor asked for is `cancelled` (whatever the run had come to), a limit is `max_turn_requests`, everything else `end_turn` (a run that
/// failed or needs the person has said so in a message).
pub fn stop_reason(outcome: &Outcome, cancelled_by_editor: bool) -> &'static str {
    if cancelled_by_editor {
        return "cancelled";
    }
    match outcome.exit_code() {
        EXIT_CANCELLED => "cancelled",
        EXIT_LIMIT => "max_turn_requests",
        _ => "end_turn",
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The server

struct Server {
    out: Out,
    log: Log,
    /// The API client, or why there is none (the token could not be read): every call that needs the core says so.
    client: Result<Client, String>,
    wait: u64,
    profile: String,
    backoff_unit: Duration,
    initialized: bool,
    sessions: HashMap<String, Session>,
    tick: u64,
    active: usize,
    tx: Sender<Msg>,
}

fn string_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params[key].as_str().ok_or_else(|| RpcError::invalid_params(&format!("{key} is a text")))
}

impl Server {
    fn client(&self) -> Result<&Client, RpcError> {
        self.client.as_ref().map_err(|why| RpcError::unauthorized(why))
    }

    fn touch(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// A line from the editor: a request (answered), a notification (not), or a response to something this process asked (nothing is asked, so it is ignored).
    fn line(&mut self, text: &str) {
        let value: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => {
                self.out.fail(&Value::Null, &RpcError::parse_error("the message is not JSON"));
                return;
            }
        };
        let Some(object) = value.as_object() else {
            let why = if value.is_array() { "batches are not supported: send one message a line" } else { "a message is a JSON object" };
            self.out.fail(&Value::Null, &RpcError::invalid_request(why));
            return;
        };
        let id = object.get("id").cloned();
        let id_ok = id.as_ref().is_none_or(|i| i.is_string() || i.is_number());
        if !id_ok {
            self.out.fail(&Value::Null, &RpcError::invalid_request("id is a number or a text"));
            return;
        }
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            self.out.fail(&id.unwrap_or(Value::Null), &RpcError::invalid_request("jsonrpc is \"2.0\""));
            return;
        }
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        match (object.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => self.request(id, method, params),
            (Some(method), None) => self.notification(method, &params),
            (None, Some(_)) if object.contains_key("result") || object.contains_key("error") => self.log.line("a response came from the editor: this agent asks the editor for nothing, so it was ignored"),
            (None, id) => {
                self.out.fail(&id.unwrap_or(Value::Null), &RpcError::invalid_request("a message has a method"));
            }
        }
    }

    fn notification(&mut self, method: &str, params: &Value) {
        if method == "session/cancel" {
            if let Some(session) = params["sessionId"].as_str().and_then(|id| self.sessions.get(id)) {
                if let Some(ctl) = &session.busy {
                    ctl.cancel.store(true, Ordering::SeqCst);
                }
            }
        }
        // Any other notification (`$/cancel_request`, one this agent does not know) is ignored, as the protocol says.
    }

    fn request(&mut self, id: Value, method: &str, params: Value) {
        if method != "initialize" && !self.initialized {
            self.out.fail(&id, &RpcError::invalid_request("send initialize first"));
            return;
        }
        let result = match method {
            "initialize" => self.initialize(&params),
            "session/new" => self.new_session(&params),
            "session/list" => self.list(&params),
            "session/load" => self.load(&params, true),
            "session/resume" => self.load(&params, false),
            "session/prompt" => match self.prompt(&id, &params) {
                // The answer comes when the run ends.
                Ok(()) => return,
                Err(e) => Err(e),
            },
            other => Err(RpcError::method_not_found(other)),
        };
        match result {
            Ok(v) => self.out.respond(&id, v),
            Err(e) => self.out.fail(&id, &e),
        };
    }

    fn initialize(&mut self, params: &Value) -> Result<Value, RpcError> {
        let asked = params["protocolVersion"].as_u64().filter(|v| *v >= 1).ok_or_else(|| RpcError::invalid_params("protocolVersion is a whole number from 1"))?;
        self.initialized = true;
        Ok(json!({
            "protocolVersion": asked.min(PROTOCOL_VERSION),
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": { "image": false, "audio": false, "embeddedContext": true },
                "mcpCapabilities": { "http": false, "sse": false },
                "sessionCapabilities": { "list": {}, "resume": {} }
            },
            "authMethods": [],
            "agentInfo": { "name": "aqua", "title": "Aqua", "version": crate::version() }
        }))
    }

    /// Makes room for one more session: the oldest one that is not going is let go (the core still has its record: `session/load` brings it back).
    fn make_room(&mut self) -> Result<(), RpcError> {
        if self.sessions.len() < MAX_SESSIONS {
            return Ok(());
        }
        let oldest = self.sessions.iter().filter(|(_, s)| s.busy.is_none()).min_by_key(|(_, s)| s.tick).map(|(id, _)| id.clone());
        match oldest {
            Some(id) => {
                self.sessions.remove(&id);
                Ok(())
            }
            None => Err(RpcError::limit(&format!("{MAX_SESSIONS} sessions are going at once"))),
        }
    }

    fn cwd_param(params: &Value) -> Result<String, RpcError> {
        let cwd = string_param(params, "cwd")?;
        if !valid_cwd(cwd) {
            return Err(RpcError::invalid_params("cwd is an absolute path (a drive letter, a UNC name or a slash) of at most 1,024 characters"));
        }
        Ok(cwd.to_string())
    }

    fn new_session(&mut self, params: &Value) -> Result<Value, RpcError> {
        let cwd = Self::cwd_param(params)?;
        self.make_room()?;
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let tick = self.touch();
        let id = format!("acp_{:x}_{ms:x}_{tick:x}", std::process::id());
        self.sessions.insert(id.clone(), Session { cwd, busy: None, tick });
        Ok(json!({ "sessionId": id }))
    }

    fn list(&mut self, params: &Value) -> Result<Value, RpcError> {
        let mut path = format!("/api/v1/sessions?limit={LIST_LIMIT}");
        if let Some(cwd) = params["cwd"].as_str().filter(|c| !c.is_empty()) {
            if !valid_cwd(cwd) {
                return Err(RpcError::invalid_params("cwd is an absolute path"));
            }
            path.push_str(&format!("&directory={}", encode(cwd)));
        }
        if let Some(cursor) = params["cursor"].as_str().filter(|c| !c.is_empty()) {
            if cursor.len() > 256 || !cursor.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'=' | b'.' | b'~')) {
                return Err(RpcError::invalid_params("cursor is the one the last answer gave"));
            }
            path.push_str(&format!("&cursor={}", encode(cursor)));
        }
        let reply = self.client()?.send("GET", &path, None).map_err(|e| RpcError::from_net(&e))?;
        if reply.status != 200 {
            let (message, code) = reply.error();
            return Err(RpcError::from_core(reply.status, &message, &code));
        }
        let body = reply.json();
        let sessions: Vec<Value> = body["sessions"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|s| {
                        let id = s["id"].as_str().filter(|i| valid_session(i))?;
                        let mut info = json!({ "sessionId": id, "cwd": s["directory"].as_str().unwrap_or("") });
                        if let Some(title) = s["title"].as_str().filter(|t| !t.is_empty()) {
                            info["title"] = json!(bounded(&clean(title), 200));
                        }
                        if let Some(at) = s["updated_at"].as_i64() {
                            info["updatedAt"] = json!(iso8601(at));
                        }
                        Some(info)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut result = json!({ "sessions": sessions });
        if let Some(next) = body["next"].as_str() {
            result["nextCursor"] = json!(next);
        }
        Ok(result)
    }

    fn load(&mut self, params: &Value, replay: bool) -> Result<Value, RpcError> {
        let id = string_param(params, "sessionId")?;
        if !valid_session(id) {
            return Err(RpcError::invalid_params("sessionId is a name the core gave this agent"));
        }
        let cwd = Self::cwd_param(params)?;
        let reply = self.client()?.send("GET", &format!("/api/v1/sessions/{id}"), None).map_err(|e| RpcError::from_net(&e))?;
        if reply.status != 200 {
            let (message, code) = reply.error();
            return Err(RpcError::from_core(reply.status, &message, &code));
        }
        let record = reply.json();
        if !self.sessions.contains_key(id) {
            self.make_room()?;
        }
        let tick = self.touch();
        let busy = self.sessions.get(id).and_then(|s| s.busy.clone());
        self.sessions.insert(id.to_string(), Session { cwd, busy, tick });
        if replay {
            let runs = record["runs"].as_array().cloned().unwrap_or_default();
            for run in runs.iter().skip(runs.len().saturating_sub(MAX_REPLAY_RUNS)) {
                for (kind, key) in [("user_message_chunk", "goal"), ("agent_message_chunk", "final_answer")] {
                    if let Some(text) = run[key].as_str().filter(|t| !t.trim().is_empty()) {
                        self.out.notify(id, json!({ "sessionUpdate": kind, "content": { "type": "text", "text": bounded(&clean(text), MAX_TEXT_BYTES) } }));
                    }
                }
            }
        }
        Ok(Value::Null)
    }

    fn prompt(&mut self, id: &Value, params: &Value) -> Result<(), RpcError> {
        let session_id = string_param(params, "sessionId")?.to_string();
        let blocks = params["prompt"].as_array().ok_or_else(|| RpcError::invalid_params("prompt is a list of content blocks"))?;
        let (goal, cut) = goal_from_prompt(blocks)?;
        let client = self.client()?.clone();
        if self.active >= MAX_PROMPTS {
            return Err(RpcError::limit(&format!("{MAX_PROMPTS} prompts are going at once")));
        }
        let tick = self.touch();
        let session = self.sessions.get_mut(&session_id).ok_or_else(|| RpcError::not_found("no such session: make one with session/new, or bring one back with session/load"))?;
        if session.busy.is_some() {
            return Err(RpcError::conflict("this session has a prompt that has not ended: wait for it, or send session/cancel"));
        }
        let ctl = Arc::new(PromptCtl::default());
        session.busy = Some(ctl.clone());
        session.tick = tick;
        let cwd = session.cwd.clone();
        self.active += 1;
        let worker = Worker { out: self.out.clone(), log: self.log.clone(), client, wait: self.wait, profile: self.profile.clone(), backoff_unit: self.backoff_unit };
        let (tx, id) = (self.tx.clone(), id.clone());
        std::thread::spawn(move || {
            let result = worker.drive(&session_id, &cwd, &goal, cut, &ctl);
            let _ = tx.send(Msg::Done { id, session: session_id, result });
        });
        Ok(())
    }

    fn done(&mut self, id: &Value, session: &str, result: Result<&'static str, RpcError>) {
        self.active = self.active.saturating_sub(1);
        if let Some(s) = self.sessions.get_mut(session) {
            s.busy = None;
        }
        match result {
            Ok(reason) => self.out.respond(id, json!({ "stopReason": reason })),
            Err(e) => self.out.fail(id, &e),
        };
    }

    /// The editor has gone: every run that is still going is asked to end.
    fn cancel_all(&mut self) {
        for s in self.sessions.values() {
            if let Some(ctl) = &s.busy {
                ctl.cancel.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// `aqua-cli acp`: speaks the protocol until the editor closes the input. The exit code is 0 after a clean end and 1 when the command line is wrong.
pub fn run(args: &[String], default_data: &Path, io: AcpIo) -> i32 {
    let log = Log { inner: Arc::new(Mutex::new(io.log)) };
    let with: Vec<&str> = COMMON.iter().copied().filter(|c| *c != "format").chain(["wait-approval", "profile"]).collect();
    let p = match parse(args.get(1..).unwrap_or(&[]), &with, &[]) {
        Ok(p) => p,
        Err(e) => {
            log.line(&e);
            return 1;
        }
    };
    if !p.rest.is_empty() {
        log.line("usage: aqua-cli acp [--core URL] [--data-dir DIR] [--token-file FILE] [--wait-approval SECS] [--profile coding|files|web|desktop]");
        return 1;
    }
    let wait = match p.value("wait-approval").map(|v| v.trim().parse::<u64>()).transpose() {
        Ok(w) => w.unwrap_or(DEFAULT_WAIT_APPROVAL),
        Err(_) => {
            log.line("--wait-approval is a whole number of seconds");
            return 1;
        }
    };
    let profile = p.value("profile").unwrap_or("coding").to_ascii_lowercase();
    if !matches!(profile.as_str(), "coding" | "files" | "web" | "desktop") {
        log.line("--profile is coding, files, web or desktop");
        return 1;
    }
    let data: PathBuf = p.value("data-dir").or(p.value("data")).map(PathBuf::from).unwrap_or_else(|| default_data.to_path_buf());
    let addr = match core_address(p.value("core")) {
        Ok(a) => a,
        Err(e) => {
            log.line(&e);
            return 1;
        }
    };
    // A token that cannot be read is not fatal: the editor can still start the agent and is told why a prompt cannot go (`unauthorized`), in the chat, where a person will see it.
    let client = read_token(TokenKind::Api, p.value("token-file").map(PathBuf::from).as_deref(), &data).map(|t| Client::new(addr, t));
    if let Err(why) = &client {
        log.line(&format!("no API token: {why}"));
    }
    if io.stdin_is_terminal {
        log.line("this speaks the Agent Client Protocol on standard input and output: an editor starts it (documentation/ACP.md). Type JSON-RPC messages, one a line, to try it by hand.");
    }
    let (tx, rx) = channel::<Msg>();
    let out = Out { inner: Arc::new(Mutex::new(io.output)) };
    let input = io.input;
    let reader_tx = tx.clone();
    std::thread::spawn(move || reader_thread(input, reader_tx));
    let mut server = Server { out, log: log.clone(), client, wait, profile, backoff_unit: io.backoff_unit, initialized: false, sessions: HashMap::new(), tick: 0, active: 0, tx };
    let mut closing: Option<Instant> = None;
    loop {
        let msg = match closing {
            None => rx.recv().ok(),
            Some(until) => rx.recv_timeout(until.saturating_duration_since(Instant::now())).ok(),
        };
        match msg {
            Some(Msg::Line(text)) => server.line(&text),
            Some(Msg::TooLong) => {
                server.out.fail(&Value::Null, &RpcError::too_large());
            }
            Some(Msg::Eof) => {
                log.line("the editor closed the input");
                server.cancel_all();
                closing = Some(Instant::now() + CLOSE_WAIT);
                if server.active == 0 {
                    break;
                }
            }
            Some(Msg::Done { id, session, result }) => {
                server.done(&id, &session, result);
                if closing.is_some() && server.active == 0 {
                    break;
                }
            }
            None => break,
        }
    }
    0
}
