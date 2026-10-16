//! A remote MCP server, spoken to over HTTP (card OA11-03, task EX-11 of the OpenCode audit; off unless `AQUA_MCP_REMOTE=on`).
//!
//! A remote server is somebody else's program on somebody else's machine, and a call to it sends what the model put in the call out of this computer. So it is not given a way out of its own:
//! **every request goes through the tool host's HTTP engine** (`aqua_native_tools::http::client`), the one the `http.request` tool and the remote-model door use, under the rules of
//! `data/tools/http.json`. What that means here:
//!
//! * the host must be one the person **allowed** in `http.json` (a host that would only be asked about is refused: the questions about a call to a server are the host's, one for every call,
//!   and the address is the person's own, written in `mcp.json`); a private or local address needs `allow_private` as for any request, and Aqua's own ports and the service ports are never reached;
//! * the connection goes only to the addresses Aqua's own resolver vetted, no redirect is followed (an answer that is one is refused), and an answer over 16 MB is refused;
//! * the headers are the ones of the entry: text, or the name of a secret in the credential store, read when the server starts, never logged, never put in a question or in an error, and masked
//!   in everything the host returns or records (`ToolHost::register_secret`, which the manager does);
//! * the trust, the lock on the tools, the labels on what it answers, the scope and the question before every call are the ones of any server of the person's (`aqua_mcp_adapters::user_servers`);
//!   this module is only the transport: [`HttpMcpClient`] is a [`McpTransport`].
//!
//! The protocol is the Streamable HTTP of the revision 2025-11-25: a POST for each message, an answer as JSON or as a stream of server-sent events (read to its end: the HTTP engine returns a
//! complete body), a session named by `Mcp-Session-Id`, and a DELETE when the server is stopped. Not built, and said so in `documentation/TOOLS_CONFIG.md`: logging in with OAuth, the stream
//! the server may open with a GET to speak first (so a change of its tools is noticed in the answer of a call, or by listing them again every five minutes), and the stateless protocol of
//! 2026-07-28 over HTTP.

use anyhow::{anyhow, bail, Result};
use aqua_mcp_adapters::stdio_mcp::{tool_name_problem, McpServerStatus, McpTool, PROTOCOL_VERSION};
use aqua_mcp_adapters::tool_lock::{self, LockReport, LockState};
use aqua_mcp_adapters::transport::McpTransport;
use aqua_mcp_adapters::user_servers::{RemoteConnector, RemoteSpec};
use aqua_native_tools::http::client::{Client, Request};
use aqua_native_tools::http::{NetPolicy, Verdict};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The most an answer may hold.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// How many requests of its own (sampling, roots...) a server may make in the answer to one request before it is cut off.
const MAX_SERVER_REQUESTS: u32 = 16;
/// The most tools taken from a listing, and the most pages of one.
const MAX_TOOLS_LISTED: usize = 1000;
const MAX_TOOL_PAGES: usize = 10;
/// A server that cannot speak first is asked for its tools again when they have not been listed for this long (the person's calls ask: a change must not wait for a restart).
pub const RELIST_AFTER: Duration = Duration::from_secs(300);
/// How long the DELETE that ends a session may take.
const DELETE_TIMEOUT: Duration = Duration::from_secs(3);

/// Are remote servers on (`AQUA_MCP_REMOTE=on`)?
pub fn enabled() -> bool {
    crate::flags::is_on(&crate::flags::MCP_REMOTE)
}

fn one_line(text: &str, max: usize) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(max).collect()
}

/// The transport of one remote server.
pub struct HttpMcpClient {
    label: String,
    url: String,
    /// The headers of the entry, secrets resolved. Never printed: this type has no `Debug`.
    headers: Vec<(String, String)>,
    timeout: Duration,
    client: Client,
    policy: Arc<NetPolicy>,
    session: Option<String>,
    protocol: String,
    initialized: bool,
    server_name: Option<String>,
    server_version: Option<String>,
    tools: Vec<McpTool>,
    rejected: Vec<String>,
    tool_lock: LockReport,
    tools_changed: bool,
    last_listed: Option<Instant>,
    allowed_calls: Option<BTreeSet<String>>,
    next_id: u64,
    max_response: usize,
}

impl HttpMcpClient {
    /// A client for the server `spec` names, under `policy`. `Err` when the address is not one the policy allows (see the module's text).
    pub fn new(policy: Arc<NetPolicy>, spec: &RemoteSpec) -> Result<Self, String> {
        let me = Self {
            label: spec.id.clone(),
            url: spec.url.clone(),
            headers: spec.headers.clone(),
            timeout: spec.timeout,
            client: Client::new(policy.clone()),
            policy,
            session: None,
            protocol: PROTOCOL_VERSION.to_string(),
            initialized: false,
            server_name: None,
            server_version: None,
            tools: Vec::new(),
            rejected: Vec::new(),
            tool_lock: LockReport::default(),
            tools_changed: false,
            last_listed: None,
            allowed_calls: None,
            next_id: 1,
            max_response: MAX_RESPONSE_BYTES,
        };
        me.check_allowed().map_err(|e| format!("{e:#}"))?;
        Ok(me)
    }

    /// A smaller limit on an answer than the 16 MB (the tests use one).
    pub fn with_max_response(mut self, bytes: usize) -> Self {
        self.max_response = bytes.min(MAX_RESPONSE_BYTES);
        self
    }

    /// The address must be one the person allowed in `http.json` and the policy does not refuse. Looked at before every request, so that an edit of the file that takes the host off the list
    /// is seen at once (and a first hop that the engine would only ask about is not taken as allowed).
    fn check_allowed(&self) -> Result<()> {
        let target = NetPolicy::parse(&self.url).map_err(|e| anyhow!("{} is not an address that can be used: {e}", self.label))?;
        match self.policy.check(&target) {
            Verdict::Allow => Ok(()),
            Verdict::Ask => bail!("{} is not on the allow list of data/tools/http.json: add it there (allow) to use this server; a remote server is not reached on a question", target.host),
            Verdict::Deny(why) => bail!("{why}"),
        }
    }

    fn lock_key(&self) -> String {
        format!("{}:{}", self.label.to_ascii_lowercase(), self.server_name.as_deref().unwrap_or("").to_ascii_lowercase())
    }

    fn server_label(&self) -> String {
        format!("{} {}", self.server_name.as_deref().unwrap_or(&self.label), self.server_version.as_deref().unwrap_or("")).trim().to_string()
    }

    fn status(&self) -> McpServerStatus {
        McpServerStatus {
            label: self.label.clone(),
            process_running: self.initialized,
            initialized: self.initialized,
            command: self.url.clone(),
            args: Vec::new(),
            tool_count: self.tools.len(),
            server_name: self.server_name.clone(),
            server_version: self.server_version.clone(),
            hint: String::new(),
            tool_lock: self.tool_lock.clone(),
            protocol: Some(format!("{} (Streamable HTTP)", self.protocol)),
            rejected_tools: self.rejected.clone(),
        }
    }

    /// One POST. `Ok` holds every JSON-RPC message of the answer (an answer that is a stream holds several; one that is empty, such as the `202` that answers a notification, none).
    fn post(&mut self, message: &Value) -> Result<Vec<Value>> {
        self.check_allowed()?;
        let mut headers: Vec<(String, String)> = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "application/json, text/event-stream".into()),
            ("MCP-Protocol-Version".into(), self.protocol.clone()),
        ];
        if let Some(session) = &self.session {
            headers.push(("Mcp-Session-Id".into(), session.clone()));
        }
        headers.extend(self.headers.iter().cloned());
        let request = Request {
            method: "POST".into(),
            url: self.url.clone(),
            headers,
            body: Some(serde_json::to_vec(message)?),
            credential: None,
            timeout: self.timeout,
            max_bytes: self.max_response,
            follow_redirects: false,
            max_redirects: 0,
        };
        let stop = AtomicBool::new(false);
        let response = self.client.execute_keeping(&request, &stop, &["mcp-session-id"]).map_err(|e| anyhow!("{}: {}", self.label, one_line(&e.message, 300)))?;
        if response.blocked_redirect.is_some() {
            bail!("{} sent the request somewhere else, and redirects are not followed", self.label);
        }
        match response.status {
            200..=299 => {}
            401 | 403 => bail!("{} refused the credentials it was sent (HTTP {})", self.label, response.status),
            404 if self.session.is_some() => {
                // The server forgot the session (it was restarted, or it expired): the next call starts a new one.
                self.initialized = false;
                self.session = None;
                self.tools_changed = false;
                bail!("the session with {} ended; it is started again by the next call", self.label);
            }
            300..=399 => bail!("{} answered with a redirect (HTTP {}), which is not followed", self.label, response.status),
            other => bail!("{} answered HTTP {other} {}: {}", self.label, one_line(&response.status_text, 40), one_line(&String::from_utf8_lossy(&response.body), 160)),
        }
        if response.truncated {
            bail!("{} sent an answer over {} bytes: it is not used", self.label, self.max_response);
        }
        if let Some((_, id)) = response.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id")) {
            if id.is_empty() || id.len() > 256 || !id.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
                bail!("{} sent a session id that is not one", self.label);
            }
            self.session = Some(id.clone());
        }
        if response.body.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(Vec::new());
        }
        let content_type = response.headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.to_ascii_lowercase()).unwrap_or_default();
        let text = String::from_utf8_lossy(&response.body);
        if content_type.contains("text/event-stream") {
            Ok(parse_events(&text))
        } else if content_type.contains("json") {
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Array(items)) => Ok(items),
                Ok(other) => Ok(vec![other]),
                Err(e) => bail!("{} sent JSON that cannot be read: {e}", self.label),
            }
        } else {
            bail!("{} answered with {} where JSON or an event stream was expected", self.label, one_line(&content_type, 60))
        }
    }

    /// A request of the server's own is refused (Aqua declares no capability for it): the answer is posted, and what the server says to it is not looked at.
    fn refuse(&mut self, id: Value) {
        let reply = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not supported by this client"}});
        let _ = self.post(&reply);
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let messages = self.post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let mut asked = 0u32;
        let mut answer: Option<Value> = None;
        for m in messages {
            if let Some(their_method) = m.get("method").and_then(|v| v.as_str()) {
                match m.get("id").cloned() {
                    Some(their_id) => {
                        asked += 1;
                        if asked > MAX_SERVER_REQUESTS {
                            bail!("{} keeps asking for capabilities this client does not have ({asked} requests while it answered {method}): stopped", self.label);
                        }
                        self.refuse(their_id);
                    }
                    None => {
                        let their_method = their_method.to_string();
                        self.on_notification(&their_method);
                    }
                }
                continue;
            }
            if m.get("id").and_then(|v| v.as_u64()) == Some(id) {
                answer = Some(m);
            }
        }
        let answer = answer.ok_or_else(|| anyhow!("{} sent no answer to {method}", self.label))?;
        if let Some(err) = answer.get("error") {
            bail!("{} error on {method}: {}", self.label, one_line(err.get("message").and_then(|v| v.as_str()).unwrap_or("MCP error"), 300));
        }
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.post(&json!({"jsonrpc": "2.0", "method": method, "params": params})).map(|_| ())
    }

    /// What a notification does. None of it reaches the model or the ledger.
    fn on_notification(&mut self, method: &str) {
        if method == "notifications/tools/list_changed" {
            self.tools_changed = true;
        }
    }
}

impl Drop for HttpMcpClient {
    /// A server that is replaced or dropped is told its session is over (best effort), as a program is ended when its client goes.
    fn drop(&mut self) {
        let _ = McpTransport::shutdown(self);
    }
}

/// The messages of a body that is a stream of server-sent events: the `data` lines of each event are one JSON text (an array is a batch).
fn parse_events(text: &str) -> Vec<Value> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    for event in text.split("\n\n") {
        let mut data = String::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if data.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&data) {
            Ok(Value::Array(items)) => out.extend(items),
            Ok(other) => out.push(other),
            Err(_) => {}
        }
        if out.len() >= 512 {
            break;
        }
    }
    out
}

impl McpTransport for HttpMcpClient {
    fn start(&mut self) -> Result<McpServerStatus> {
        if self.initialized {
            return Ok(self.status());
        }
        self.session = None;
        self.protocol = PROTOCOL_VERSION.to_string();
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                // Nothing is declared: the server may not ask for sampling, roots or questions.
                "capabilities": {},
                "clientInfo": {"name": format!("aqua-{}", self.label.to_ascii_lowercase().replace(' ', "-")), "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        if let Some(info) = result.get("serverInfo") {
            self.server_name = info.get("name").and_then(|v| v.as_str()).map(|s| one_line(s, 80));
            self.server_version = info.get("version").and_then(|v| v.as_str()).map(|s| one_line(s, 40));
        }
        if let Some(spoken) = result.get("protocolVersion").and_then(|v| v.as_str()).filter(|v| v.len() <= 20 && v.chars().all(|c| c.is_ascii_digit() || c == '-')) {
            self.protocol = spoken.to_string();
        }
        self.initialized = true;
        let started = (|| -> Result<()> {
            self.notify("notifications/initialized", json!({}))?;
            let tools = self.list_tools()?;
            self.tool_lock = tool_lock::observe(&self.lock_key(), &self.server_label(), &tools);
            if self.tool_lock.state == LockState::Changed {
                tracing::warn!("{}: {}", self.label, self.tool_lock.summary());
            }
            Ok(())
        })();
        if let Err(e) = started {
            let _ = self.shutdown();
            return Err(e);
        }
        Ok(self.status())
    }

    fn is_running(&self) -> bool {
        self.initialized
    }

    fn is_initialized(&self) -> bool {
        self.initialized
    }

    fn cached_tools(&self) -> &[McpTool] {
        &self.tools
    }

    fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        if !self.initialized {
            bail!("{} is not started. Call start() first.", self.label);
        }
        self.tools_changed = false;
        let mut tools: Vec<McpTool> = Vec::new();
        let mut rejected: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let result = self.request("tools/list", params)?;
            if let Some(items) = result.get("tools").and_then(|v| v.as_array()) {
                for t in items {
                    let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if let Some(why) = tool_name_problem(&name) {
                        rejected.push(format!("{}: {why}", one_line(&name, 40)));
                        continue;
                    }
                    if tools.iter().any(|known| known.name == name) {
                        rejected.push(format!("{}: listed twice", one_line(&name, 40)));
                        continue;
                    }
                    if tools.len() >= MAX_TOOLS_LISTED {
                        rejected.push(format!("{}: more than {MAX_TOOLS_LISTED} tools", one_line(&name, 40)));
                        continue;
                    }
                    tools.push(McpTool {
                        name,
                        description: t.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        input_schema: t.get("inputSchema").cloned().unwrap_or(json!({})),
                        source: Some(self.label.to_ascii_lowercase()),
                    });
                }
            }
            let next = result.get("nextCursor").and_then(|v| v.as_str()).filter(|c| !c.is_empty()).map(str::to_string);
            if next.is_none() || next == cursor {
                break;
            }
            cursor = next;
        }
        // The same order every time, as the program kind of server's list is.
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        self.rejected = rejected;
        self.tools = tools.clone();
        self.last_listed = Some(Instant::now());
        Ok(tools)
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        if !self.initialized {
            bail!("{} is not started. Call start() first.", self.label);
        }
        if let Some(allowed) = &self.allowed_calls {
            if !allowed.contains(name) {
                bail!("{} is not one of the tools of {} that may be called", one_line(name, 60), self.label);
            }
        }
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}))?;
        if result.get("resultType").and_then(|v| v.as_str()) == Some("input_required") {
            bail!("{} needs something from this client before it can run {}, which Aqua does not provide", self.label, one_line(name, 60));
        }
        if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            let msg = result.get("content").and_then(|c| c.as_array()).and_then(|a| a.first()).and_then(|i| i.get("text")).and_then(|t| t.as_str()).unwrap_or("MCP tool returned an error");
            bail!("{msg}");
        }
        Ok(result)
    }

    fn shutdown(&mut self) -> Result<()> {
        if let (true, Some(session)) = (self.initialized, self.session.clone()) {
            // Best effort: the server is told the session is over. An error, a refusal or a server that is gone is of no interest now.
            if self.check_allowed().is_ok() {
                let mut headers = vec![("Mcp-Session-Id".to_string(), session), ("MCP-Protocol-Version".to_string(), self.protocol.clone())];
                headers.extend(self.headers.iter().cloned());
                let request = Request { method: "DELETE".into(), url: self.url.clone(), headers, body: None, credential: None, timeout: DELETE_TIMEOUT, max_bytes: 4096, follow_redirects: false, max_redirects: 0 };
                let _ = self.client.execute(&request, &AtomicBool::new(false));
            }
        }
        self.initialized = false;
        self.session = None;
        self.tools.clear();
        self.tool_lock = LockReport::default();
        self.server_name = None;
        self.server_version = None;
        self.tools_changed = false;
        self.last_listed = None;
        Ok(())
    }

    fn take_tools_changed(&mut self) -> bool {
        std::mem::take(&mut self.tools_changed)
    }

    fn drain_notifications(&mut self) {
        // A server that cannot speak first (this transport opens no stream) is asked again for its tools now and then, so that a change of them is seen without waiting for a call that carries the news.
        if self.initialized && self.last_listed.is_some_and(|t| t.elapsed() >= RELIST_AFTER) {
            self.tools_changed = true;
        }
    }

    fn approve_tools(&mut self) -> bool {
        let accepted = tool_lock::approve(&self.lock_key(), &self.server_label(), &self.tools);
        if accepted {
            self.tool_lock = LockReport { state: LockState::Unchanged, ..Default::default() };
        }
        accepted
    }

    fn forget_recorded_tools(&mut self) {
        tool_lock::forget(&self.lock_key());
        self.tool_lock = LockReport::default();
    }

    fn restrict_calls_to(&mut self, names: Vec<String>) {
        self.allowed_calls = Some(names.into_iter().collect());
    }
}

/// The transport for `spec` under `policy`, or why there is none.
pub fn transport_for(policy: Arc<NetPolicy>, spec: &RemoteSpec) -> Result<Box<dyn McpTransport>, String> {
    HttpMcpClient::new(policy, spec).map(|c| Box::new(c) as Box<dyn McpTransport>)
}

/// What the manager of the servers is given (`ServerManager::set_remote_connector`): it reads `http.json` (with `load_http_config`, the tool host's own reader, which the tool host hands
/// over) each time a server starts, so the rules are the ones of the file as it is then, and refuses every server unless remote servers are on.
pub fn connector(data_dir: PathBuf, load_http_config: fn(&std::path::Path) -> Option<aqua_native_tools::http::HttpConfig>) -> RemoteConnector {
    Arc::new(move |spec: &RemoteSpec| {
        if !enabled() {
            return Err("remote MCP servers are off: set AQUA_MCP_REMOTE=on".to_string());
        }
        let Some(cfg) = load_http_config(&data_dir) else {
            return Err("data/tools/http.json cannot be read, so the rules for reaching another machine are not known and no remote server is used until it is fixed".to_string());
        };
        transport_for(Arc::new(NetPolicy::new(cfg)), spec)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_that_is_a_stream_of_events_is_read_to_its_messages() {
        let body = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\r\n\r\n: a comment\n\nid: 7\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":3,\"result\":{}}\n\ndata: [{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{}},{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}]\n\ndata: not json\n\n";
        let messages = parse_events(body);
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert_eq!(messages[0]["method"], "notifications/tools/list_changed");
        assert_eq!(messages[1]["id"], 3, "the lines of one event are one text");
        assert_eq!((messages[2]["id"].clone(), messages[3]["id"].clone()), (json!(4), json!(5)), "a batch is its messages");
        assert!(parse_events("").is_empty() && parse_events("event: ping\n\n").is_empty());
        let many = "data: {\"id\":1}\n\n".repeat(2000);
        assert!(parse_events(&many).len() <= 512 + 1, "an endless stream does not make an endless list");
    }
}
