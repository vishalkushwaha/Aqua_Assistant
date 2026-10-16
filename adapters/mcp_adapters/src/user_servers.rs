//! MCP servers the person adds in `data/tools/mcp.json`, in the notation other programs use:
//!
//! ```json
//! {"mcpServers": {
//!   "notes": {"command": "npx.cmd", "args": ["-y", "some-notes-server"], "env": {"NOTES_DIR": "D:/notes", "NOTES_KEY": {"secret": "notes-key"}},
//!             "tools_allow": ["add_note", "list_*"], "read_only": ["list_*"], "timeout_ms": 30000}
//! }}
//! ```
//!
//! Each tool of a server becomes a host tool `mcp_<id>.<tool>` and so goes through the one door: the host decides, asks, labels the answer as
//! untrusted text, masks secrets and writes the ledger. What is different about them is how little they are trusted:
//!
//! * a server is **not started until the person trusts it** (the app shows the exact command line), and the trust is for that command line
//!   and those variable names: a change to the entry asks again; the tools it lists are recorded (`tool_lock`) and a server whose tools
//!   changed since is not used until the person has looked;
//! * every call **asks** (a tool's default is Ask, and it counts as an effect on another system unless the person listed it in `read_only`);
//!   what a server says about itself (a hint such as "read only") is never believed;
//! * what a server wrote about its tools (descriptions) is **never shown to the model**: it is text somebody else wrote, and a poisoned
//!   description is the usual way in. The model gets the tool's name and its typed arguments;
//! * its answer is untrusted text, and counts as private to the person;
//! * it starts from an empty environment (a short list of names, and what the entry sets), never the API token or anything that looks like a
//!   credential, directly and not through a shell.
//!
//! What card OA11-03 (task EX-10, EX-11 and EX-13 of the OpenCode audit) adds, all of it only ever taking away:
//!
//! * **scope and exposure.** `"scope": {"projects": ["E:/work/*"], "agents": ["build"]}` makes a server usable only by the runs of those projects and agents: it is not shown to any other run
//!   and the tool host refuses its calls (`classify_for` reads the run), so naming a tool that was not shown gets nowhere. `"expose": "named"|"ranked"|"always"` is how its tools are shown
//!   (the default is today's rule: the goal names the server); that is a matter of what the model is shown, never of what it may call.
//! * **a server from a repository** (`.aqua/mcp.json` of a pack whose code the person trusted by hash) is a server of that project only: it is listed, **not started**, until the person trusts it
//!   by itself, its tools are asked about every time, it cannot call itself a read, and it cannot widen its exposure or its scope.
//! * **`tools/list_changed`.** The tools are listed again when the server says they changed; a tool that is **new** or whose description or arguments differ from what the person accepted is
//!   quarantined (not served, named in the list of servers) until the person trusts the server again, and the others go on working.
//! * **a restart budget** (`restart_budget`): a server that keeps ending is started again three times in ten minutes and then not any more.
//! * **a remote server** (`"url": "https://..."`, only with `AQUA_MCP_REMOTE=on`): spoken to over HTTP through the tool host's own engine (`core/src/mcp_remote.rs`), so the allow-list, the guard
//!   against private addresses and the cap on a response apply; headers hold a secret by its name in the credential store.

use crate::restart_budget::RestartBudget;
use crate::stdio_mcp::{McpTool, StdioMcpClient, StdioMcpConfig};
use crate::tool_lock::LockState;
use crate::transport::McpTransport;
use aqua_tool_host::provider::ClassifyContext;
use aqua_tool_host::{
    hex_lower, Annotations, Availability, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

pub const MAX_SERVERS: usize = 16;
/// The longest command line (the program and its arguments) that a server may have: the question that asks the person to trust it shows all of it, and a command line is never
/// trusted on a question that hides part of it. (It used to be cut at 300 characters, with 64 arguments of 1024 allowed: most of a command could not be seen.)
pub const MAX_COMMAND_LINE: usize = 1500;
/// The longest literal value of an environment variable of a server: shown whole beside the command line.
const MAX_ENV_VALUE: usize = 500;
/// The most characters of a call's arguments that the question about the call can show.
const MAX_SHOWN_ARGS: usize = 1500;
/// How many tools of one server are used (the rest are left out).
pub const MAX_TOOLS: usize = 40;
const MAX_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpFile {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<String, ServerEntry>,
}

/// A variable of the server's environment: a plain value, or the name of a secret in the credential store (read when the server starts).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum EnvValue {
    Literal(String),
    Secret { secret: String },
}

/// How the tools of a server are shown to a run (card OA11-03). It decides what the model is shown and never what it may call: the tool host decides that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Expose {
    /// The goal names the server (as a word): its tools are shown. What it has always been, and the default.
    #[default]
    Named,
    /// The tools whose names and argument names fit the goal are shown (the best eight of all the servers that say so), whether the goal names the server or not.
    Ranked,
    /// All its tools are shown to every run it is in scope for (counted in the eight a prompt shows). Costs prompt tokens: only for a server the person wants always at hand.
    Always,
}

impl Expose {
    pub fn as_str(self) -> &'static str {
        match self {
            Expose::Named => "named",
            Expose::Ranked => "ranked",
            Expose::Always => "always",
        }
    }
}

/// Which runs a server is for (card OA11-03). An empty list means every run. A scope only narrows: a run outside it is not shown the server's tools, and the tool host refuses its calls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// Folders (a path, or a path with `*` for any run of characters): the runs whose project is one of them or inside one. Spelt as the person likes (`E:\work\*`, `e:/work/app`).
    #[serde(default)]
    pub projects: Vec<String>,
    /// Names of agents (`build`, `plan`, ...): a run that is not asked to be an agent is `build`.
    #[serde(default)]
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerEntry {
    /// The program to start. Empty for a remote server (`url`).
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, EnvValue>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The tools of this server that may be used: exact names or a prefix with a star. Empty means all of them.
    #[serde(default)]
    pub tools_allow: Vec<String>,
    /// The tools the person says only read (same patterns). The others count as an effect on another system and always ask.
    #[serde(default)]
    pub read_only: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// How the tools are shown to a run (card OA11-03).
    #[serde(default)]
    pub expose: Expose,
    /// Which runs the server is for (card OA11-03).
    #[serde(default)]
    pub scope: Scope,
    /// A remote server: `https://host/path` (plain `http` only to this computer). Needs `AQUA_MCP_REMOTE=on`; the host must be one the tool host's HTTP rules allow (`http.json`). Not with `command`.
    #[serde(default)]
    pub url: Option<String>,
    /// The headers a remote server is sent: a literal value, or the name of a secret in the credential store. A header that carries a credential must be a secret.
    #[serde(default)]
    pub headers: BTreeMap<String, EnvValue>,
}

impl ServerEntry {
    /// Is this a remote server (an `https` address) and not a program?
    pub fn is_remote(&self) -> bool {
        self.url.is_some()
    }
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// The longest address of a remote server.
const MAX_URL_CHARS: usize = 500;
/// Headers of a remote server, and the longest value of a literal one.
const MAX_HEADERS: usize = 16;
/// How many projects and agents a scope lists.
const MAX_SCOPE_ITEMS: usize = 16;

/// A header the person's file may not set: the protocol's own, and the ones that would make the request something else.
fn reserved_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "content-length" | "content-type" | "accept" | "connection" | "transfer-encoding" | "upgrade" | "te" | "trailer" | "expect" | "mcp-session-id" | "mcp-protocol-version" | "user-agent" | "origin" | "referer"
    )
}

/// A header that carries a credential has to come from the credential store: a token written into `mcp.json` would be in a file that is read, copied and committed.
fn credential_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(n.as_str(), "authorization" | "proxy-authorization" | "cookie" | "x-api-key" | "api-key") || ["token", "secret", "password", "apikey", "api-key", "auth"].iter().any(|w| n.contains(w))
}

/// A header name as HTTP spells one: letters, digits and `-` (nothing that would end the header or start another).
fn valid_header_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Is this the address of a server on this computer (the only kind that may be reached without `https`)?
fn is_local_host(host: &str) -> bool {
    let h = host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    h == "localhost" || h == "127.0.0.1" || h == "::1"
}

/// Why an address is not one a remote server may have, or `None`.
pub fn url_problem(raw: &str) -> Option<String> {
    if raw.chars().count() > MAX_URL_CHARS || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Some(format!("the address is over {MAX_URL_CHARS} characters or has a space or a control character in it"));
    }
    let Ok(parsed) = url::Url::parse(raw) else { return Some("the address is not a web address".into()) };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Some("the address has a user name or password in it: a login belongs in a header that names a secret".into());
    }
    if parsed.fragment().is_some() || parsed.query().is_some_and(|q| !q.is_empty()) {
        return Some("the address has a query or a fragment: a key in an address ends up in logs and in the person's file; use a header that names a secret".into());
    }
    let Some(host) = parsed.host_str() else { return Some("the address has no host".into()) };
    match parsed.scheme() {
        "https" => None,
        "http" if is_local_host(host) => None,
        "http" => Some("plain http is for this computer only (localhost, 127.0.0.1); another machine is reached with https".into()),
        other => Some(format!("{other}: is not a scheme a remote server is reached with (https)")),
    }
}

/// A path as it is compared: forward slashes, no `\\?\`, no trailing slash, lower case (a Windows path has no case).
pub fn normalise_path(path: &str) -> String {
    let mut s = path.trim().replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    while s.len() > 1 && s.ends_with('/') && !s.ends_with(":/") {
        s.pop();
    }
    s.to_lowercase()
}

/// Does `pattern` (normalised, with `*` for any run of characters, `/` included) fit `text`? With no `*` a pattern fits the folder it names and everything inside it.
fn glob_fits(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return text == pattern || text.strip_prefix(pattern).is_some_and(|rest| rest.starts_with('/'));
    }
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// What the host knows of the run a call belongs to, for the scope of a server (card OA11-03).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunInfo {
    /// The folder of the project the run works in. `None`: the run has no project (a call outside a run has none).
    pub project: Option<String>,
    /// The agent the run is asked to be. `None`: a plain run, which is `build`.
    pub agent: Option<String>,
}

/// Looks a run up by its id (empty for a call outside a run). Set by the core, which knows the projects and the agents.
pub type RunInfoFn = Arc<dyn Fn(&str) -> RunInfo + Send + Sync>;

/// The rule that says which runs a server is for: its scope, and for a server from a repository the project it belongs to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeRule {
    projects: Vec<String>,
    agents: Vec<String>,
    /// For a server from a repository's pack: the folder of that project (normalised). A run outside it never sees the server.
    owner: Option<String>,
}

impl ScopeRule {
    /// The rule of an entry. `owner` is the project folder of a server from a pack: its own `scope.projects` is not read (it is the project, and nothing wider); `scope.agents` narrows it further.
    pub fn of(entry: &ServerEntry, owner: Option<&str>) -> Self {
        Self {
            projects: if owner.is_some() { Vec::new() } else { entry.scope.projects.iter().map(|p| normalise_path(p)).collect() },
            agents: entry.scope.agents.iter().map(|a| a.trim().to_ascii_lowercase()).collect(),
            owner: owner.map(normalise_path),
        }
    }

    /// Every run is in scope.
    pub fn is_open(&self) -> bool {
        self.projects.is_empty() && self.agents.is_empty() && self.owner.is_none()
    }

    /// May this run use the server? `Err` says why not, in a sentence for the person.
    pub fn allows(&self, run: &RunInfo) -> Result<(), String> {
        let project = run.project.as_deref().map(normalise_path);
        if let Some(owner) = &self.owner {
            match &project {
                Some(p) if p == owner || p.strip_prefix(owner.as_str()).is_some_and(|rest| rest.starts_with('/')) => {}
                _ => return Err("this server belongs to another project: it is for the runs of the repository that has it in its .aqua folder".into()),
            }
        } else if !self.projects.is_empty() {
            match &project {
                Some(p) if self.projects.iter().any(|g| glob_fits(g, p)) => {}
                _ => return Err("this server is turned on for some projects only, and this run is not in one of them".into()),
            }
        }
        if !self.agents.is_empty() {
            let agent = run.agent.as_deref().map(str::to_ascii_lowercase).unwrap_or_else(|| "build".into());
            if !self.agents.contains(&agent) {
                return Err(format!("this server is turned on for some agents only ({}), and this run is {agent}", self.agents.join(", ")));
            }
        }
        Ok(())
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 24 && id.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Does a pattern of the person's (an exact name, or a prefix and a star) name this tool? Tool names are case-sensitive (`Delete` and `delete` are two tools), so a pattern for
/// one is not a pattern for the other: it matters most for `read_only`, where a match means fewer questions.
fn pattern_matches(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| match p.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => p == name,
    })
}

/// Read the file's text: every server with its id. `Err` says what is wrong (the whole file is refused, so a mistake never leaves a server
/// more open than meant).
pub fn parse_file(text: &str) -> Result<BTreeMap<String, ServerEntry>, String> {
    // Comments and trailing commas are accepted, as in every config file (`aqua_tool_host::config_text`); a position in an error is the one in the person's file.
    let text = aqua_tool_host::config_text::prepare(text).map_err(|e| e.detail())?;
    let file: McpFile = serde_json::from_str(&text).map_err(|e| format!("not valid: {e}"))?;
    if file.servers.len() > MAX_SERVERS {
        return Err(format!("{} servers, at most {MAX_SERVERS}", file.servers.len()));
    }
    for (id, e) in &file.servers {
        if !valid_id(id) {
            return Err(format!("server {id:?}: the name is 1 to 24 of a-z, 0-9 and _, starting with a letter"));
        }
        validate_remote_and_scope(id, e)?;
        let cmd = e.command.trim();
        if !e.is_remote() && (cmd.is_empty() || cmd.len() > 260 || cmd.contains(['\0', '\n', '\r'])) {
            return Err(format!("server {id}: the command is a program name or path"));
        }
        if e.args.len() > 64 || e.args.iter().any(|a| a.contains('\0') || a.len() > 1024) {
            return Err(format!("server {id}: too many or too long arguments"));
        }
        if e.env.len() > 32 || e.env.keys().any(|k| k.is_empty() || k.len() > 64 || k.contains(['=', '\0', ' '])) {
            return Err(format!("server {id}: an environment variable has a bad name"));
        }
        let line_length = raw_command_line(e).chars().count();
        if line_length > MAX_COMMAND_LINE {
            return Err(format!("server {id}: the command line is {line_length} characters and the question that asks you to trust it can show {MAX_COMMAND_LINE}: it is not trusted on a question that hides part of it"));
        }
        if e.env.values().any(|v| matches!(v, EnvValue::Literal(s) if s.chars().count() > MAX_ENV_VALUE)) || e.cwd.as_ref().is_some_and(|c| c.chars().count() > MAX_ENV_VALUE) {
            return Err(format!("server {id}: a value or the working folder is over {MAX_ENV_VALUE} characters: it is shown whole before you trust the server, so keep it shorter (a secret belongs in the credential store)"));
        }
        if e.env.values().any(|v| matches!(v, EnvValue::Secret { secret } if secret.trim().is_empty() || secret.len() > 128)) {
            return Err(format!("server {id}: a secret needs the name of one in the credential store"));
        }
        if e.timeout_ms == 0 || e.timeout_ms > MAX_TIMEOUT_MS {
            return Err(format!("server {id}: timeout_ms is 1 to {MAX_TIMEOUT_MS}"));
        }
        if e.tools_allow.iter().chain(e.read_only.iter()).any(|p| p.trim().is_empty() || p.len() > 64) {
            return Err(format!("server {id}: a tool pattern is empty or too long"));
        }
    }
    Ok(file.servers)
}

/// What a remote server's entry and the scope of any entry have to be (card OA11-03): a server is a program or an address and not both; an address is `https` (or `http` to this computer),
/// has no login or key in it, and its headers either hold a secret by name or are nothing that carries a credential; a scope lists names of a sensible size.
fn validate_remote_and_scope(id: &str, e: &ServerEntry) -> Result<(), String> {
    match &e.url {
        Some(url) => {
            if !e.command.trim().is_empty() || !e.args.is_empty() || !e.env.is_empty() || e.cwd.is_some() {
                return Err(format!("server {id}: a remote server has an url and none of command, args, env or cwd (it is not a program Aqua starts)"));
            }
            if let Some(why) = url_problem(url) {
                return Err(format!("server {id}: {why}"));
            }
            if e.headers.len() > MAX_HEADERS {
                return Err(format!("server {id}: at most {MAX_HEADERS} headers"));
            }
            for (name, value) in &e.headers {
                if !valid_header_name(name) {
                    return Err(format!("server {id}: {name:?} is not a header name (letters, digits and - only)"));
                }
                if reserved_header(name) {
                    return Err(format!("server {id}: the header {name} is the protocol's own and is not set from the file"));
                }
                match value {
                    EnvValue::Literal(text) => {
                        if credential_header(name) {
                            return Err(format!("server {id}: the header {name} carries a credential: give it as {{\"secret\": \"name\"}}, the name of a secret in the credential store, not as text in this file"));
                        }
                        if text.chars().count() > MAX_ENV_VALUE || text.chars().any(|c| c.is_control()) {
                            return Err(format!("server {id}: the value of the header {name} is over {MAX_ENV_VALUE} characters or has a control character in it"));
                        }
                    }
                    EnvValue::Secret { secret } => {
                        if secret.trim().is_empty() || secret.len() > 128 {
                            return Err(format!("server {id}: a secret needs the name of one in the credential store"));
                        }
                    }
                }
            }
        }
        None => {
            if !e.headers.is_empty() {
                return Err(format!("server {id}: headers belong to a remote server (an url)"));
            }
        }
    }
    if e.scope.projects.len() > MAX_SCOPE_ITEMS || e.scope.agents.len() > MAX_SCOPE_ITEMS {
        return Err(format!("server {id}: a scope lists at most {MAX_SCOPE_ITEMS} projects and {MAX_SCOPE_ITEMS} agents"));
    }
    if e.scope.projects.iter().any(|p| p.trim().is_empty() || p.chars().count() > 200 || p.chars().any(|c| c.is_control())) {
        return Err(format!("server {id}: a project in the scope is empty, over 200 characters or has a control character in it"));
    }
    if e.scope.agents.iter().any(|a| a.is_empty() || a.len() > 64 || !a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')) {
        return Err(format!("server {id}: an agent in the scope is a name of 1 to 64 of a-z, 0-9, - and _"));
    }
    Ok(())
}

/// What the person's trust is for: the server's name, command, arguments, working folder, its variables (their values, which are in the
/// file for anyone to read; for a secret its name, never its value) and which tools are allowed and called reads. A change to any of them is a
/// different server: `NODE_OPTIONS=--require something.js` changes what a program does as much as its arguments do.
///
/// Card OA11-03: the address and the headers of a remote server, its scope and its exposure are part of it too, **only when they are set**, so that the fingerprint of an entry that has none
/// of them is what it always was and nobody who trusted a server has to trust it again.
pub fn fingerprint(id: &str, e: &ServerEntry) -> String {
    let env: Vec<(&String, String)> = e.env.iter().map(|(k, v)| (k, match v { EnvValue::Secret { secret } => format!("secret:{secret}"), EnvValue::Literal(s) => format!("value:{s}") })).collect();
    let mut canon = json!({"id": id, "command": e.command.trim(), "args": e.args, "cwd": e.cwd, "env": env, "tools_allow": e.tools_allow, "read_only": e.read_only});
    if let Some(map) = canon.as_object_mut() {
        if let Some(url) = &e.url {
            let headers: Vec<(&String, String)> = e.headers.iter().map(|(k, v)| (k, match v { EnvValue::Secret { secret } => format!("secret:{secret}"), EnvValue::Literal(s) => format!("value:{s}") })).collect();
            map.insert("url".into(), json!(url));
            map.insert("headers".into(), json!(headers));
        }
        if e.expose != Expose::Named {
            map.insert("expose".into(), json!(e.expose.as_str()));
        }
        if e.scope != Scope::default() {
            map.insert("scope".into(), json!({"projects": e.scope.projects, "agents": e.scope.agents}));
        }
    }
    Sha256::digest(canon.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The command line: the program and its arguments, with quotes where there are spaces. For a remote server: the address and the names of the headers it sends.
fn raw_command_line(e: &ServerEntry) -> String {
    if let Some(url) = &e.url {
        let headers = if e.headers.is_empty() { String::new() } else { format!(" (sends the headers {})", e.headers.keys().cloned().collect::<Vec<_>>().join(", ")) };
        return format!("remote server {url}{headers}");
    }
    let quote = |a: &str| if a.is_empty() || a.contains([' ', '\t', '"']) { format!("\"{}\"", a.replace('"', "\\\"")) } else { a.to_string() };
    std::iter::once(e.command.trim()).chain(e.args.iter().map(String::as_str)).map(quote).collect::<Vec<_>>().join(" ")
}

/// The command line as the person is shown it before they trust a server: all of it (`parse_file` refuses one that is too long to show), on one line, with line breaks and
/// invisible or direction-changing characters shown as marks and by their number.
pub fn command_line(e: &ServerEntry) -> String {
    let line = raw_command_line(e);
    aqua_tool_host::shown_whole(&line, MAX_COMMAND_LINE).unwrap_or_else(|n| format!("[a command line of {n} characters: too long to be shown, so it cannot be trusted]"))
}

/// The program to start: a bare name is looked for in the folders of `PATH` with the extensions Windows runs (`npx` is `npx.cmd`), so the
/// server can be started directly, with no shell between.
pub fn resolve_program(command: &str, path_var: &str, pathext: &str) -> String {
    let c = command.trim();
    if c.contains(['/', '\\']) || c.contains(':') {
        return c.to_string();
    }
    let exts: Vec<String> = if std::path::Path::new(c).extension().is_some() { vec![String::new()] } else { std::iter::once(String::new()).chain(pathext.split(';').filter(|e| e.starts_with('.')).map(|e| e.to_ascii_lowercase())).collect() };
    for dir in path_var.split(';').filter(|d| !d.is_empty()) {
        for ext in &exts {
            let candidate = std::path::Path::new(dir).join(format!("{c}{ext}"));
            if candidate.is_file() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }
    c.to_string()
}

/// The name a server's tool gets in the host: `mcp_<id>.<tool>`, with the tool's own name cut to lower-case letters, digits and `_`.
pub fn host_tool_name(id: &str, wire: &str, taken: &[String]) -> String {
    let mut part = String::new();
    for c in wire.to_ascii_lowercase().chars() {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '_' };
        if c != '_' || !part.ends_with('_') {
            part.push(c);
        }
    }
    let mut part = part.trim_matches('_').to_string();
    if part.is_empty() {
        part = "tool".into();
    }
    if part.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        part.insert_str(0, "t_");
    }
    part.truncate(40);
    let mut name = format!("mcp_{id}.{part}");
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("mcp_{id}.{part}_{n}");
        n += 1;
    }
    name
}

/// The text of an MCP result: its text parts, and a note for what is not text.
pub fn render_result(result: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = result.get("content").and_then(|c| c.as_array()) {
        for item in items {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => parts.push(item.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                Some("image") => parts.push(format!("[an image of {} bytes was returned and is not shown]", item.get("data").and_then(|d| d.as_str()).map_or(0, |d| d.len() * 3 / 4))),
                Some("resource") | Some("resource_link") => {
                    let uri = item.get("resource").and_then(|r| r.get("uri")).or_else(|| item.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                    parts.push(format!("[a resource: {}]", uri.chars().take(200).collect::<String>()));
                }
                Some(other) => parts.push(format!("[a part of type {}]", other.chars().filter(|c| c.is_ascii_alphanumeric()).take(20).collect::<String>())),
                None => {}
            }
        }
    }
    if parts.is_empty() {
        return match result.get("structuredContent") {
            Some(v) => v.to_string(),
            None => "Done.".into(),
        };
    }
    parts.join("\n")
}

fn one_line(text: &str, max: usize) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

/// A tool of a server as the host serves it.
#[derive(Debug, Clone)]
pub struct ServedTool {
    pub host_name: String,
    pub wire_name: String,
    /// What the server wrote (cut to one line): shown to the person in the app, never to the model.
    pub description: String,
    pub schema: Value,
    pub read_only: bool,
    /// The tool is on another machine (a remote server): it also counts as a use of the network.
    pub network: bool,
}

/// A tool of a server that is not served because its schema cannot be (card OA5-04, TL-09): the rest of the server is. What the person is shown instead of the tool.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SkippedTool {
    /// The server's own name for the tool, cut to plain characters (it is somebody else's text).
    pub name: String,
    /// Why, in one line.
    pub reason: String,
}

/// A server that has been started, with the tools it offers.
pub struct Connected {
    pub provider: McpProvider,
    pub server_name: Option<String>,
    pub tools: Vec<ServedTool>,
    /// The tools it lists that are left out, and why.
    pub skipped: Vec<SkippedTool>,
    /// How many of the tools it lists are not served for a reason the person did not choose (a schema that cannot be served, a name that cannot be used, more tools than are used): the
    /// ones the person's own `tools_allow` leaves out are not counted (card OA11-03).
    pub omitted_tools: usize,
}

/// What a remote server is spoken to with: made by the core, which holds the HTTP engine and its rules (card OA11-03, task EX-11).
#[derive(Clone)]
pub struct RemoteSpec {
    pub id: String,
    pub url: String,
    /// The headers, with the secrets read from the credential store: never logged, never shown.
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
}

impl std::fmt::Debug for RemoteSpec {
    /// The names of the headers and never their values: a `{:?}` of this in a log or a failed assertion must not carry a credential.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSpec").field("id", &self.id).field("url", &self.url).field("headers", &self.headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()).field("timeout", &self.timeout).finish()
    }
}

/// Makes the transport of a remote server, or says why it cannot be (remote servers are off, the host is not on the allow-list, a private address). Without one every remote entry fails to start.
pub type RemoteConnector = Arc<dyn Fn(&RemoteSpec) -> Result<Box<dyn McpTransport>, String> + Send + Sync>;

/// Everything about how a server is connected that is not in its entry.
#[derive(Clone, Default)]
pub struct ConnectOptions {
    /// Makes the transport of a remote server.
    pub remote: Option<RemoteConnector>,
    /// The folder of the project a server from a pack belongs to. A server from a pack is held to more: its tools always ask, it cannot call a tool a read, and it cannot widen its exposure.
    pub project: Option<String>,
    /// Looks up the run of a call, for the scope.
    pub run_info: Option<RunInfoFn>,
    /// A budget of its own for restarts (the tests use short ones).
    pub budget: Option<RestartBudget>,
}

/// What changes while a server runs: which of its tools are held back, whether it has been given up on.
#[derive(Debug, Default)]
struct Live {
    /// Tools that were served and are not any more (by the server's own name), with why.
    blocked: BTreeMap<String, String>,
    /// Tools the server lists now that nobody accepted: never served.
    new_tools: BTreeSet<String>,
    /// The server was given up on (it ended too often): why.
    failure: Option<String>,
    /// A change was announced and listing the tools again did not work: the next call tries again.
    relist_pending: bool,
}

/// What the provider's calls (which run on a blocking thread) and the provider itself share.
struct Shared {
    id: String,
    client: Mutex<Box<dyn McpTransport>>,
    enabled: Arc<AtomicBool>,
    live: Mutex<Live>,
    budget: Mutex<RestartBudget>,
    /// Every tool the server listed when it was accepted, by the server's own name, with the hash of its name, description and schema.
    accepted: BTreeMap<String, String>,
    /// The server's own names of the tools that are served.
    served: BTreeSet<String>,
}

impl Shared {
    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Compares a listing with what the person accepted: a tool that is served and is gone or different is held back; one that was never accepted is named and never served.
    fn apply_listing(&self, tools: &[McpTool]) {
        let now: BTreeMap<&str, String> = tools.iter().map(|t| (t.name.as_str(), crate::tool_lock::fingerprint(t))).collect();
        let mut blocked = BTreeMap::new();
        for wire in &self.served {
            match now.get(wire.as_str()) {
                None => {
                    blocked.insert(wire.clone(), "the server does not list it any more".to_string());
                }
                Some(hash) if self.accepted.get(wire) != Some(hash) => {
                    blocked.insert(wire.clone(), "its description or its arguments changed since you trusted the server: look at the server and trust it again".to_string());
                }
                Some(_) => {}
            }
        }
        let new_tools: BTreeSet<String> = now.keys().filter(|n| !self.accepted.contains_key(**n)).map(|n| n.to_string()).collect();
        if !blocked.is_empty() || !new_tools.is_empty() {
            tracing::warn!("MCP server {}: its tools changed since you trusted it ({} held back, {} new and not used)", self.id, blocked.len(), new_tools.len());
        }
        let mut live = self.live();
        live.blocked = blocked;
        live.new_tools = new_tools;
        live.relist_pending = false;
    }

    /// Lists the tools again and compares them. A listing that fails is tried again at the next call.
    fn relist(&self, client: &mut dyn McpTransport) {
        match client.list_tools() {
            Ok(tools) => self.apply_listing(&tools),
            Err(e) => {
                tracing::warn!("MCP server {}: its tools could not be listed again: {}", self.id, one_line(&format!("{e:#}"), 200));
                self.live().relist_pending = true;
            }
        }
    }

    fn held_back(&self, wire: &str) -> Option<String> {
        self.live().blocked.get(wire).cloned()
    }
}

pub struct McpProvider {
    id: String,
    provider_id: String,
    command_line: String,
    shared: Arc<Shared>,
    tools: Vec<ServedTool>,
    skipped: Vec<SkippedTool>,
    omitted: usize,
    scope: ScopeRule,
    run_info: Option<RunInfoFn>,
    /// Every call asks and no answer is kept for the session: why. For a server from a repository, and for a remote one.
    always_ask: Option<String>,
}

/// The manifest the host is given for a tool of the server `server_id`. Never more than the person said: a server cannot make itself a read by calling itself one. What the server
/// wrote about the tool is not repeated (see the module's note).
fn manifest_of(server_id: &str, t: &ServedTool) -> ToolManifest {
    let mut classes = vec![PermClass::Remote];
    if t.network {
        classes.push(PermClass::Network);
    }
    if !t.read_only {
        classes.push(PermClass::ExternalEffect);
    }
    ToolManifest {
        name: t.host_name.clone(),
        title: t.host_name.clone(),
        description: format!("A tool of the MCP server {server_id} you added: {}.", plain_name(&t.wire_name)),
        classes,
        default_decision: Decision::Ask,
        input_schema: t.schema.clone(),
        timeout_default_ms: 30_000,
        timeout_max_ms: MAX_TIMEOUT_MS,
        annotations: Annotations { read_only: t.read_only, destructive: false, idempotent: t.read_only, open_world: true },
        untrusted_output: true,
    }
}

/// The tool with its schema as the host takes it ([`schema_inline::normalise`]), or why it cannot be served: the schema cannot be made one, or the host's check of the manifest
/// (`ToolManifest::check`, which stays as strict as it was) refuses it.
fn served_with_schema(server_id: &str, mut tool: ServedTool, listed: &Value) -> Result<ServedTool, String> {
    tool.schema = crate::schema_inline::normalise(listed)?;
    if let Err(why) = manifest_of(server_id, &tool).check() {
        let prefix = format!("{}: ", tool.host_name);
        return Err(why.strip_prefix(&prefix).unwrap_or(&why).to_string());
    }
    Ok(tool)
}

/// A tool's own name as plain characters: letters, digits, `_`, `-` and `.`, at most 60 (it is a stranger's text and is shown to the person and to the model).
fn plain_name(wire: &str) -> String {
    wire.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')).take(60).collect()
}

fn client_config(id: &str, e: &ServerEntry, resolve_secret: &dyn Fn(&str) -> Option<String>) -> Result<StdioMcpConfig, String> {
    let mut env = Vec::new();
    for (k, v) in &e.env {
        match v {
            EnvValue::Literal(s) => env.push((k.clone(), s.clone())),
            EnvValue::Secret { secret } => {
                let value = resolve_secret(secret).ok_or_else(|| format!("the secret '{secret}' for {k} is not in the credential store"))?;
                env.push((k.clone(), value));
            }
        }
    }
    let command = resolve_program(&e.command, &std::env::var("PATH").unwrap_or_default(), &std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into()));
    Ok(StdioMcpConfig {
        label: id.to_string(),
        command,
        args: e.args.clone(),
        cwd: e.cwd.as_ref().map(std::path::PathBuf::from),
        timeout: Duration::from_millis(e.timeout_ms),
        ready_hint: String::new(),
        start_hint: String::new(),
        // Started directly, from an empty environment: no shell, and nothing the person did not name.
        direct_spawn: true,
        env_clear: true,
        env_allow: Vec::new(),
        env,
        must_stay_on_loopback: None,
        // Whatever the person added: nothing is known about it, so it is asked.
        known_legacy: false,
    })
}

/// The headers a remote server is sent, with the secrets read from the credential store (a secret that is not there stops the start, and says which).
fn resolve_headers(e: &ServerEntry, resolve_secret: &dyn Fn(&str) -> Option<String>) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (name, value) in &e.headers {
        match value {
            EnvValue::Literal(s) => out.push((name.clone(), s.clone())),
            EnvValue::Secret { secret } => {
                let v = resolve_secret(secret).ok_or_else(|| format!("the secret '{secret}' for the header {name} is not in the credential store"))?;
                if v.chars().any(|c| c.is_control()) {
                    return Err(format!("the secret '{secret}' for the header {name} has a line break or a control character in it"));
                }
                out.push((name.clone(), v));
            }
        }
    }
    Ok(out)
}

impl McpProvider {
    /// Start the server and read its tools. With `accept` (the person trusts the server now) the tools it lists become the record to compare
    /// with; without it they must be the recorded ones, or the server is refused (its tools changed, or were never accepted).
    pub fn connect(id: &str, entry: &ServerEntry, accept: bool, resolve_secret: &dyn Fn(&str) -> Option<String>) -> Result<Connected, String> {
        Self::connect_with(id, entry, accept, resolve_secret, &ConnectOptions::default())
    }

    /// [`connect`](McpProvider::connect) for a server that is not the plain kind (card OA11-03): a remote one, one from a repository's pack, one with a scope or a budget of its own.
    pub fn connect_with(id: &str, entry: &ServerEntry, accept: bool, resolve_secret: &dyn Fn(&str) -> Option<String>, options: &ConnectOptions) -> Result<Connected, String> {
        if !valid_id(id) {
            return Err(format!("{id:?} is not a valid server name"));
        }
        let from_pack = options.project.is_some();
        let mut client: Box<dyn McpTransport> = match &entry.url {
            None => Box::new(StdioMcpClient::new(client_config(id, entry, resolve_secret)?)),
            Some(url) => {
                if from_pack {
                    return Err("a server from a repository's .aqua folder is a program on this computer and cannot be a remote one".into());
                }
                let Some(connector) = options.remote.as_ref() else {
                    return Err("remote MCP servers are off: set AQUA_MCP_REMOTE=on (and list the host in http.json) to use one".into());
                };
                let headers = resolve_headers(entry, resolve_secret)?;
                connector(&RemoteSpec { id: id.to_string(), url: url.clone(), headers, timeout: Duration::from_millis(entry.timeout_ms) })?
            }
        };
        let status = client.start().map_err(|e| format!("{e:#}"))?;
        if accept {
            if !client.approve_tools() {
                return Err("the tools it lists could not be recorded".into());
            }
        } else {
            match status.tool_lock.state {
                LockState::Unchanged => {}
                LockState::Changed => return Err(format!("its tools changed since you trusted it ({}): look at them and trust it again", status.tool_lock.summary())),
                LockState::New | LockState::Unchecked => {
                    // Looking at it recorded its tools; nobody accepted them, so that is taken back.
                    client.forget_recorded_tools();
                    return Err("its tools were never accepted: trust it first".into());
                }
            }
        }
        // What the person accepted: every tool the server lists now, by its own name, with the hash of its name, description and schema. A later listing is compared with this, in memory,
        // whatever the lock file says.
        let listed: Vec<McpTool> = client.cached_tools().to_vec();
        let accepted: BTreeMap<String, String> = listed.iter().map(|t| (t.name.clone(), crate::tool_lock::fingerprint(t))).collect();
        let mut served: Vec<ServedTool> = Vec::new();
        let mut skipped: Vec<SkippedTool> = Vec::new();
        let mut taken: Vec<String> = Vec::new();
        let mut over_limit = 0usize;
        for t in listed.iter().take(256) {
            if !entry.tools_allow.is_empty() && !pattern_matches(&entry.tools_allow, &t.name) {
                continue;
            }
            if served.len() >= MAX_TOOLS {
                over_limit += 1;
                continue;
            }
            // A tool's host name does not depend on whether another tool of the server is left out: a name that a recipe or a rule uses stays the same.
            let host_name = host_tool_name(id, &t.name, &taken);
            taken.push(host_name.clone());
            // A server from a repository cannot call a tool a read: the file that says so is the repository's, not the person's.
            let read_only = !from_pack && pattern_matches(&entry.read_only, &t.name);
            let tool = ServedTool { host_name, wire_name: t.name.clone(), description: one_line(&t.description, 300), schema: Value::Null, read_only, network: entry.is_remote() };
            // The schema as the host takes it (references put in), and the host's own check on the manifest that would be registered: a tool that fails either is left out and named, and
            // the others are served (`ToolHost::register` refuses a whole provider for one manifest that fails).
            match served_with_schema(id, tool, &t.input_schema) {
                Ok(tool) => served.push(tool),
                Err(reason) => {
                    let name = plain_name(&t.name);
                    tracing::warn!("MCP server {id}: the tool {name} is left out: {reason}");
                    skipped.push(SkippedTool { name, reason });
                }
            }
        }
        let omitted_tools = skipped.len() + over_limit + listed.len().saturating_sub(256) + status.rejected_tools.len();
        if over_limit > 0 {
            tracing::warn!("MCP server {id}: {over_limit} tool(s) beyond the {MAX_TOOLS} that are used are left out");
        }
        // Only the tools that are served may be called, whatever asks: the client refuses any other name itself.
        client.restrict_calls_to(served.iter().map(|t| t.wire_name.clone()).collect());
        let always_ask = if from_pack {
            Some("this server comes from a repository's .aqua folder: every call to it asks".to_string())
        } else if entry.is_remote() {
            Some("this server is on another machine: every call to it asks".to_string())
        } else {
            None
        };
        let shared = Arc::new(Shared {
            id: id.to_string(),
            client: Mutex::new(client),
            enabled: Arc::new(AtomicBool::new(true)),
            live: Mutex::new(Live::default()),
            budget: Mutex::new(options.budget.clone().unwrap_or_default()),
            accepted,
            served: served.iter().map(|t| t.wire_name.clone()).collect(),
        });
        let provider = McpProvider {
            id: id.to_string(),
            provider_id: format!("mcp_{id}"),
            command_line: command_line(entry),
            shared,
            tools: served.clone(),
            skipped: skipped.clone(),
            omitted: omitted_tools,
            scope: ScopeRule::of(entry, options.project.as_deref()),
            run_info: options.run_info.clone(),
            always_ask,
        };
        Ok(Connected { provider, server_name: status.server_name, tools: served, skipped, omitted_tools })
    }

    /// Stop using the server: its calls are refused and the process is ended.
    pub fn disable(&self) {
        self.shared.enabled.store(false, Ordering::SeqCst);
        if let Ok(mut c) = self.shared.client.lock() {
            let _ = c.shutdown();
        }
    }

    pub fn enabled_flag(&self) -> Arc<AtomicBool> {
        self.shared.enabled.clone()
    }

    pub fn tools(&self) -> &[ServedTool] {
        &self.tools
    }

    /// The tools of the server that are left out, with the reason for each.
    pub fn skipped(&self) -> &[SkippedTool] {
        &self.skipped
    }

    /// How many tools the server lists that are not served for a reason the person did not choose (see [`Connected::omitted_tools`]).
    pub fn omitted_tools(&self) -> usize {
        self.omitted
    }

    /// The tools that are not served although the server lists them, because they changed or are new since the person accepted the server (card OA11-03): `{name, reason}`, by the
    /// server's own name. The person trusting the server again accepts them.
    pub fn quarantined(&self) -> Vec<SkippedTool> {
        let live = self.shared.live();
        let held = live.blocked.iter().map(|(name, why)| SkippedTool { name: plain_name(name), reason: why.clone() });
        let new = live.new_tools.iter().map(|name| SkippedTool { name: plain_name(name), reason: "the server added it after you trusted it: it is not used until you trust the server again".into() });
        held.chain(new).collect()
    }

    /// Why the server was given up on (it ended too often), when it was.
    pub fn failure(&self) -> Option<String> {
        self.shared.live().failure.clone()
    }

    /// How many times the server was started again in the budget's window.
    pub fn restarts_used(&self) -> usize {
        self.shared.budget.lock().unwrap_or_else(PoisonError::into_inner).used(std::time::Instant::now())
    }

    /// Which runs the server is for.
    pub fn scope(&self) -> &ScopeRule {
        &self.scope
    }

    fn served(&self, host_name: &str) -> Option<&ServedTool> {
        self.tools.iter().find(|t| t.host_name == host_name)
    }

    /// May the run of a call use this server? A server with a scope is refused to every run outside it, and to a call outside any run.
    fn check_scope(&self, run: &str) -> Result<(), ToolError> {
        if self.scope.is_open() {
            return Ok(());
        }
        let info = self.run_info.as_ref().map(|f| f(run)).unwrap_or_default();
        self.scope.allows(&info).map_err(|why| ToolError::new(ErrorCode::PermissionDenied, format!("the MCP server {}: {why}", self.id)))
    }

    fn classify_inner(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let t = self.served(tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        // The question shows all of the arguments or the call is not made: what it hides is where the data that leaves would be put.
        let shown = aqua_tool_host::shown_whole(&args.to_string(), MAX_SHOWN_ARGS).map_err(|n| {
            ToolError::new(ErrorCode::InvalidArguments, format!("the arguments are {n} characters long and a question can show {MAX_SHOWN_ARGS}: the call is not made on a question that would hide part of them"))
        })?;
        let wire = aqua_tool_host::shown_whole(&t.wire_name, 200).unwrap_or_default();
        Ok(Classification {
            classes: vec![],
            // Not on any list: every call asks, and the question says which program it would reach (the exact command line).
            target: format!("MCP server {}", self.id),
            summary: format!("Call {wire} on the MCP server {} ({}: {}) with {shown}", self.id, if t.network { "it is at" } else { "it runs" }, self.command_line),
            target_allowed: false,
        })
    }
}

impl Shared {
    /// The wait before a server that ended is started again, ended early by a stop of the call. `false`: the call was stopped.
    fn wait_unless_stopped(wait: Duration, cancel: &AtomicBool) -> bool {
        let until = std::time::Instant::now() + wait;
        while std::time::Instant::now() < until {
            if cancel.load(Ordering::SeqCst) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25).min(until.saturating_duration_since(std::time::Instant::now())));
        }
        !cancel.load(Ordering::SeqCst)
    }

    /// One call to the server, on a thread that may wait for it (one call at a time to a server).
    fn run_call(&self, wire: &str, args: Value, cancel: &AtomicBool) -> Result<Value, ToolError> {
        let id = &self.id;
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("the MCP server {id} is not trusted any more")));
        }
        if let Some(why) = self.live().failure.clone() {
            return Err(ToolError::new(ErrorCode::Unavailable, format!("the MCP server {id} is not used: {why}")));
        }
        let mut c = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        // (Card OA4-03, TL-08.) A call that waited for the one before it, and was stopped or timed out while it waited, does not reach the server: nobody would read what it did.
        if cancel.load(Ordering::SeqCst) {
            return Err(ToolError::new(ErrorCode::Cancelled, format!("the call to {id} was stopped before it reached the server")));
        }
        // What the server said while nobody asked (that its tools changed, a log line), and whether it is still there.
        c.drain_notifications();
        if !c.is_running() || !c.is_initialized() {
            // It was stopped (a call that timed out ends it, and so does a crash): it starts again if its budget says so, and what it lists must still be what was accepted.
            let permit = self.budget.lock().unwrap_or_else(PoisonError::into_inner).permit(std::time::Instant::now());
            let wait = match permit {
                Ok(wait) => wait,
                Err(why) => {
                    let _ = c.shutdown();
                    self.live().failure = Some(why.clone());
                    tracing::warn!("MCP server {id}: {why}");
                    return Err(ToolError::new(ErrorCode::Unavailable, format!("the MCP server {id} is not used: {why}")));
                }
            };
            if !Self::wait_unless_stopped(wait, cancel) {
                return Err(ToolError::new(ErrorCode::Cancelled, format!("the call to {id} was stopped before it reached the server")));
            }
            c.start().map_err(|e| ToolError::new(ErrorCode::BackendError, one_line(&format!("{e:#}"), 300)))?;
            let listed: Vec<McpTool> = c.cached_tools().to_vec();
            self.apply_listing(&listed);
        } else if c.take_tools_changed() || self.live().relist_pending {
            self.relist(&mut **c);
        }
        if let Some(why) = self.held_back(wire) {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("the tool {} of the MCP server {id} is not used: {why}", plain_name(wire))));
        }
        let result = c.call_tool(wire, args).map_err(|e| ToolError::new(ErrorCode::BackendError, one_line(&format!("{e:#}"), 400)));
        // A server that says its tools changed while it answered is looked at at once: the next call finds a list that was compared.
        if c.take_tools_changed() {
            self.relist(&mut **c);
        }
        result
    }
}

impl ToolProvider for McpProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        self.tools.iter().map(|t| manifest_of(&self.id, t)).collect()
    }

    fn availability(&self, tool: &str) -> Availability {
        if !self.shared.enabled.load(Ordering::SeqCst) {
            return Availability::Unavailable(format!("the MCP server {} is not trusted any more", self.id));
        }
        if let Some(why) = self.shared.live().failure.clone() {
            return Availability::Unavailable(format!("the MCP server {} is not used: {why}", self.id));
        }
        // A tool that changed since the person accepted it is not offered (the host leaves out what is not available).
        if let Some(why) = self.served(tool).and_then(|t| self.shared.held_back(&t.wire_name)) {
            return Availability::Unavailable(format!("held back: {why}"));
        }
        Availability::Available
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        // Outside a run: a server with a scope is for the runs of that scope and a call that is not in any run is not one of them.
        self.check_scope("")?;
        self.classify_inner(tool, args)
    }

    fn classify_for(&self, tool: &str, args: &Value, ctx: &ClassifyContext) -> Result<Classification, ToolError> {
        self.check_scope(&ctx.run)?;
        self.classify_inner(tool, args)
    }

    fn always_ask_reason(&self, _tool: &str, _args: &Value) -> Option<String> {
        self.always_ask.clone()
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let wire = self.served(tool).map(|t| t.wire_name.clone());
        let cancel = ctx.cancel.clone();
        let shared = self.shared.clone();
        let id_out = self.id.clone();
        Box::pin(async move {
            let wire = wire.ok_or_else(|| ToolError::new(ErrorCode::NotFound, "unknown tool"))?;
            let wire_out = wire.clone();
            let done = tokio::task::spawn_blocking(move || shared.run_call(&wire, args, &cancel))
                .await
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the call stopped unexpectedly: {e}")))??;
            // What another program says is the person's data as far as anything else is concerned: it is private, and (by the manifest) untrusted.
            Ok(ProviderOutput { data: json!({"server": id_out, "tool": wire_out}), text: render_result(&done), ..Default::default() }.marked_private())
        })
    }
}

/// Describe a tool of a server for the list the person sees.
pub fn describe(tool: &McpTool) -> Value {
    json!({"name": tool.name, "description": one_line(&tool.description, 300)})
}


// ---- the servers the person has added, and which of them are trusted -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", content = "why", rename_all = "snake_case")]
pub enum Standing {
    /// In `mcp.json` and not trusted: nothing of it is started.
    NotTrusted,
    Starting,
    /// Started, and its tools are in the host.
    Ready,
    /// Trusted, and could not be used (it did not start, or its tools changed since they were accepted).
    Failed(String),
}

/// What the app shows of a server: the exact command line, whether it is trusted, and its tools (with what the server wrote about them,
/// for the person to read: the model never sees that).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServerView {
    pub id: String,
    pub command_line: String,
    /// The working folder, as written.
    pub cwd: Option<String>,
    /// The variables the server is started with: a literal value is shown whole, a secret by its name (never its value). The trust is for these too.
    pub env: Vec<Value>,
    /// The tools that may be used (empty: all of them) and the ones the person called reads: the trust is for these lists as well.
    pub tools_allow: Vec<String>,
    pub read_only: Vec<String>,
    pub trusted: bool,
    #[serde(flatten)]
    pub standing: Standing,
    pub tools: Vec<Value>,
    /// The tools the server lists that are left out because their schema cannot be served (`{name, reason}`): the rest of the server is used (card OA5-04, TL-09).
    pub skipped: Vec<SkippedTool>,
    /// The name the person gave the server in the file (the id of a server from a repository's pack has the project in it).
    pub name: String,
    /// `user` (the person's own `mcp.json`) or `project` (a repository's `.aqua/mcp.json`, trusted by hash and then by itself).
    pub source: &'static str,
    /// The project folder of a server from a pack, as it was found.
    pub project: Option<String>,
    /// `program` (started and spoken to on its standard input and output) or `remote` (an address).
    pub kind: &'static str,
    /// The address of a remote server, and the names (never the values) of the headers it is sent.
    pub url: Option<String>,
    pub header_names: Vec<String>,
    /// How its tools are shown to a run, and which runs it is for (card OA11-03).
    pub expose: Expose,
    pub scope: Scope,
    /// How many of the tools it lists are not served for a reason the person did not choose (a schema that cannot be served, a name that cannot be used, more than are used).
    pub omitted_tools: usize,
    /// The tools it lists that are not used because they are new or differ from what the person accepted (the server said its list changed): `{name, reason}`.
    pub quarantined: Vec<SkippedTool>,
    /// How many times it was started again within the budget's window (three are allowed in ten minutes).
    pub restarts: usize,
}

/// What the manager says about all its servers in a few numbers (the doctor, the status).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Summary {
    pub servers: usize,
    pub ready: usize,
    pub not_trusted: usize,
    pub failed: usize,
    pub project_servers: usize,
    pub remote_servers: usize,
    pub omitted_tools: usize,
    pub quarantined_tools: usize,
}

impl Summary {
    /// The numbers of these views.
    pub fn of(views: &[ServerView]) -> Summary {
        Summary {
            servers: views.len(),
            ready: views.iter().filter(|v| v.standing == Standing::Ready).count(),
            not_trusted: views.iter().filter(|v| v.standing == Standing::NotTrusted).count(),
            failed: views.iter().filter(|v| matches!(v.standing, Standing::Failed(_))).count(),
            project_servers: views.iter().filter(|v| v.source == "project").count(),
            remote_servers: views.iter().filter(|v| v.kind == "remote").count(),
            omitted_tools: views.iter().map(|v| v.omitted_tools).sum(),
            quarantined_tools: views.iter().map(|v| v.quarantined.len()).sum(),
        }
    }
}

/// How a server's tools are shown and who may use them, for the layer that decides what a run is shown (`core/src/mcp_exposure.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposure {
    /// The name the person gave the server: the word of a goal that names it.
    pub name: String,
    pub expose: Expose,
    pub scope: ScopeRule,
}

/// The project a server from a pack belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    /// The folder, normalised for comparing.
    key: String,
    /// The folder as it was found, for showing.
    shown: String,
    /// The name the pack gave the server.
    name: String,
}

struct Slot {
    entry: ServerEntry,
    standing: Standing,
    provider: Option<Arc<McpProvider>>,
    /// Set for a server from a repository's pack.
    binding: Option<Binding>,
}

/// What [`ServerManager::sync_project`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProjectSync {
    /// The servers that were as the pack says and were left as they are.
    pub kept: Vec<String>,
    /// Servers that are new (listed, not started).
    pub added: Vec<String>,
    /// Servers whose entry is not the one they had: stopped, and back not trusted.
    pub changed: Vec<String>,
    /// Servers the pack does not have any more: stopped and gone.
    pub removed: Vec<String>,
    /// The new servers, which [`ServerManager::start_servers`] starts if the person trusted them as they are written.
    pub to_start: Vec<String>,
}

/// What [`ServerManager::reload`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Reload {
    pub kept: Vec<String>,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    /// Servers a wrong file had stopped that are as they were trusted: [`ServerManager::start_servers`] starts them again.
    pub to_start: Vec<String>,
}

/// The most servers one project's pack may add, and how long a name in a pack is (the id gets a prefix that says which project it is).
pub const MAX_PROJECT_SERVERS: usize = 8;
pub const MAX_PROJECT_SERVER_NAME: usize = 16;

/// The id of the server `name` of the pack in `project_key` (a normalised folder): `p` and six hex digits of the folder's hash, then the name. Two projects can each have a server called
/// `gh`, and neither is the other's.
pub fn project_server_id(project_key: &str, name: &str) -> String {
    let hash: String = Sha256::digest(project_key.as_bytes()).iter().take(3).map(|b| format!("{b:02x}")).collect();
    format!("p{hash}_{name}")
}

#[derive(Debug, Default, serde::Serialize, Deserialize)]
struct TrustFile {
    /// 2 when the tags are keyed (`hmac:...`, card R6-16); absent (0) or 1 for the first format, the plain fingerprint of the entry, which anybody who can write the file can
    /// work out for an entry of their own.
    #[serde(default)]
    version: u32,
    #[serde(default)]
    servers: BTreeMap<String, String>,
}

/// HMAC-SHA-256 (RFC 2104), with the `sha2` that is here already.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    let mut tag = [0u8; 32];
    tag.copy_from_slice(&outer.finalize());
    tag
}

/// Two tags are the same, compared in a time that does not depend on where they first differ.
fn same_tag(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

pub type SecretResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The servers of `data/tools/mcp.json` and the person's trust in them (`data/tools/mcp_trusted.json`: the server's name and the fingerprint of
/// the entry it was trusted for). Nothing is started until it is trusted; a trusted server is started again when Aqua starts, only if its entry
/// is still the one that was trusted and its tools are still the ones that were accepted.
pub struct ServerManager {
    dir: std::path::PathBuf,
    host: Arc<aqua_tool_host::ToolHost>,
    resolve: SecretResolver,
    servers: Mutex<BTreeMap<String, Slot>>,
    /// The key that makes the record of trust something a file writer cannot make (see [`ServerManager::set_trust_key`]).
    key: Mutex<Option<Vec<u8>>>,
    /// How the run of a call is looked up, for the scope of a server (card OA11-03).
    run_info: Mutex<Option<RunInfoFn>>,
    /// Makes the transport of a remote server (card OA11-03); none means remote servers are off.
    remote: Mutex<Option<RemoteConnector>>,
    /// A budget for restarts that the servers started from now on are given instead of the default (the tests use a short one).
    budget: Mutex<Option<RestartBudget>>,
}

impl ServerManager {
    pub fn config_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("mcp.json")
    }

    pub fn trust_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("mcp_trusted.json")
    }

    /// Read `mcp.json` from the tools folder `dir`. A missing file is no servers; a wrong one is an `Err` that says what is wrong.
    pub fn open(dir: &std::path::Path, host: Arc<aqua_tool_host::ToolHost>, resolve: SecretResolver) -> Result<Arc<Self>, String> {
        let entries = match std::fs::read_to_string(Self::config_path(dir)) {
            Ok(text) => parse_file(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(format!("unreadable: {e}")),
        };
        let servers = entries.into_iter().map(|(id, entry)| (id, Slot { entry, standing: Standing::NotTrusted, provider: None, binding: None })).collect();
        Ok(Arc::new(Self {
            dir: dir.to_path_buf(),
            host,
            resolve,
            servers: Mutex::new(servers),
            key: Mutex::new(None),
            run_info: Mutex::new(None),
            remote: Mutex::new(None),
            budget: Mutex::new(None),
        }))
    }

    /// How a call's run is looked up, for the scope of the servers (set by the core, which knows the projects and the agents). Without it a server that has a scope is refused to every call.
    pub fn set_run_info(&self, f: RunInfoFn) {
        *self.run_info.lock().unwrap_or_else(PoisonError::into_inner) = Some(f);
    }

    /// Lets remote servers be started: `connector` makes the transport (the core's, which goes through the tool host's HTTP engine). Without it a remote entry does not start.
    pub fn set_remote_connector(&self, connector: RemoteConnector) {
        *self.remote.lock().unwrap_or_else(PoisonError::into_inner) = Some(connector);
    }

    /// A budget for restarts for the servers that are started from now on (for tests; the default is [`RestartBudget::default`]).
    pub fn set_restart_budget(&self, budget: RestartBudget) {
        *self.budget.lock().unwrap_or_else(PoisonError::into_inner) = Some(budget);
    }

    fn options_for(&self, binding: Option<&Binding>) -> ConnectOptions {
        ConnectOptions {
            remote: self.remote.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            project: binding.map(|b| b.key.clone()),
            run_info: self.run_info.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            budget: self.budget.lock().unwrap_or_else(PoisonError::into_inner).clone(),
        }
    }

    /// The key for the record of trust, kept by the credential store of this machine (card R6-16). With it the record holds, for each server, an HMAC of the entry's fingerprint,
    /// which whoever can only write `mcp_trusted.json` cannot make: the plain fingerprint was the same for them as for Aqua, so a file with a line of their own for a server of their
    /// own was a server the person had trusted.
    ///
    /// `key_is_new` is true only at the start that made the key, and only then is a record in the first format converted: what it holds is what the person trusted before there was a
    /// key, an entry that has not changed stays trusted (in the keyed form) and the rest is dropped. It is not true at any later start, because the file is the one thing a file
    /// writer can change: with the file put back to the first format and a plain line for a server of their own, converting it at every start would trust that server. With a key that
    /// is not new, a record in the first format trusts nothing, is replaced by an empty keyed one and says so in the log; and with a key, a plain fingerprint is never accepted, whatever
    /// version the file says it is.
    pub fn set_trust_key(&self, key: Vec<u8>, key_is_new: bool) {
        *self.key.lock().unwrap_or_else(PoisonError::into_inner) = Some(key);
        let old = self.trusted();
        if old.version >= 2 {
            return;
        }
        if !key_is_new {
            if !old.servers.is_empty() {
                tracing::warn!("the record of trusted MCP servers was in its first format although this machine has had a key for it: nothing in it is trusted, and each server has to be trusted again");
            }
            if let Err(e) = self.save_trusted(&TrustFile { version: 2, servers: BTreeMap::new() }) {
                tracing::warn!("the record of trusted MCP servers could not be replaced by an empty keyed one: {e}");
            }
            return;
        }
        let entries: BTreeMap<String, ServerEntry> = self.lock().iter().map(|(id, slot)| (id.clone(), slot.entry.clone())).collect();
        let mut servers = BTreeMap::new();
        for (id, plain) in &old.servers {
            if let Some(entry) = entries.get(id).filter(|e| *plain == fingerprint(id, e)) {
                if let Some(tag) = self.tag_for(id, entry, 2) {
                    servers.insert(id.clone(), tag);
                }
            }
        }
        if let Err(e) = self.save_trusted(&TrustFile { version: 2, servers }) {
            tracing::warn!("the record of trusted MCP servers could not be converted to the keyed form: {e}");
        }
    }

    /// The record as it is to be added to: one still in the first format, on a machine that has a key, starts again (what it holds was converted when the key was set, or was not
    /// an entry that is unchanged).
    fn trusted_for_writing(&self) -> TrustFile {
        let t = self.trusted();
        if t.version < 2 && self.trust_key().is_some() {
            return TrustFile { version: 2, servers: BTreeMap::new() };
        }
        t
    }

    fn trust_key(&self) -> Option<Vec<u8>> {
        self.key.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Has the key that signs the record of trust been given ([`set_trust_key`](Self::set_trust_key))?
    pub fn has_trust_key(&self) -> bool {
        self.trust_key().is_some()
    }

    /// The tag that records trust in this entry as it is now, for a record of `file_version`: keyed when there is a key; the plain fingerprint when there is none and the record
    /// is in the first format (a machine with no credential store: a test); and nothing when the record is keyed and there is no key (nothing is trusted, not everything).
    fn tag_for(&self, id: &str, entry: &ServerEntry, file_version: u32) -> Option<String> {
        let fp = fingerprint(id, entry);
        match self.trust_key() {
            Some(key) => Some(format!("hmac:{}", hex_lower(&hmac_sha256(&key, format!("aqua-mcp-trust-v2\n{fp}").as_bytes())))),
            None if file_version < 2 => Some(fp),
            None => None,
        }
    }

    fn is_trusted(&self, t: &TrustFile, id: &str, entry: &ServerEntry) -> bool {
        match (t.servers.get(id), self.tag_for(id, entry, t.version)) {
            (Some(have), Some(want)) => same_tag(have, &want),
            _ => false,
        }
    }

    fn trusted(&self) -> TrustFile {
        std::fs::read_to_string(Self::trust_path(&self.dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// Written whole and flushed (`fsutil::write_atomic_durable`): a crash used to leave a truncated record, which reads as "nothing is trusted" and loses every decision.
    fn save_trusted(&self, t: &TrustFile) -> Result<(), String> {
        let text = serde_json::to_string_pretty(t).map_err(|e| e.to_string())?;
        aqua_tool_host::fsutil::write_atomic_durable(&Self::trust_path(&self.dir), text.as_bytes()).map_err(|e| format!("the trust record could not be written: {e}"))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Slot>> {
        self.servers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_standing(&self, id: &str, standing: Standing) {
        if let Some(s) = self.lock().get_mut(id) {
            s.standing = standing;
        }
    }

    /// Start the servers that are trusted, one after the other (in the background: it can take a while). A server whose entry changed since
    /// it was trusted is left alone and says so.
    pub fn start_trusted(self: &Arc<Self>) {
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("mcp-start".into()).spawn(move || me.start_trusted_now(&|| false));
        if let Err(e) = spawned {
            tracing::warn!("could not start the thread that starts MCP servers: {e}");
        }
    }

    /// What [`start_trusted`](ServerManager::start_trusted) does on its thread, for a caller that runs it on a thread of its own (the core's job registry does, so that the start is listed
    /// and the shutdown can stop it): the trusted servers are started one after the other, and `stop` is asked before each one. A server that is being started is waited for; it is not
    /// interrupted.
    pub fn start_trusted_now(&self, stop: &dyn Fn() -> bool) {
        let trust = self.trusted();
        let ids: Vec<String> = self.lock().keys().cloned().collect();
        for id in ids {
            if stop() {
                return;
            }
            self.start_if_trusted(&trust, &id);
        }
    }

    /// Starts one server if the person trusted it as it is written now; a server whose trust is for another entry is left alone and says so.
    fn start_if_trusted(&self, trust: &TrustFile, id: &str) {
        let Some(entry) = self.lock().get(id).map(|s| s.entry.clone()) else { return };
        if !trust.servers.contains_key(id) {
            return;
        }
        if !self.is_trusted(trust, id, &entry) {
            self.set_standing(id, Standing::Failed("its entry in mcp.json changed since you trusted it, or the record of trust is not one this machine made: look at it and trust it again".into()));
            return;
        }
        self.set_standing(id, Standing::Starting);
        match self.bring_up(id, &entry, false) {
            Ok(()) => {}
            Err(why) => self.set_standing(id, Standing::Failed(why)),
        }
    }

    /// Starts the servers `ids` that the person trusted (one after the other, waiting for each), for the caller that runs it on a thread of its own after [`sync_project`](Self::sync_project)
    /// or [`reload`](Self::reload) said which servers are to be started.
    pub fn start_servers(&self, ids: &[String]) {
        let trust = self.trusted();
        for id in ids {
            self.start_if_trusted(&trust, id);
        }
    }

    /// Stops and forgets a server's slot: its calls are refused, it is ended and its tools leave the host. Not the record of trust: that is for the person to take back (`untrust`).
    fn drop_slot(&self, id: &str) {
        let slot = self.lock().remove(id);
        if let Some(provider) = slot.and_then(|s| s.provider) {
            provider.disable();
        }
        self.host.unregister(&format!("mcp_{id}"));
    }

    /// Makes the servers of the project folder `root` what `mcp_json` says, which is the text of the pack's `mcp.json` **when the pack's code is trusted** (card OA11-03, task EX-10: a
    /// server from a repository). `None`: the pack has none, or its code is not trusted (or not any more): every server of the project goes.
    ///
    /// Nothing is started here. A server of a pack is listed (`not trusted`) until the person trusts it by itself, which starts it; one the person trusted before, with an entry that is
    /// still what was trusted, is the one thing [`start_servers`](Self::start_servers) starts afterwards (the ids to start are in the answer). A changed server is not the one that was
    /// trusted: it goes, and comes back not trusted. A file that cannot be read (too many servers, an address, a name) is refused whole: the project has no servers, and the answer says why.
    pub fn sync_project(&self, root: &str, mcp_json: Option<&[u8]>) -> Result<ProjectSync, String> {
        let key = normalise_path(root);
        let parsed: Result<BTreeMap<String, ServerEntry>, String> = match mcp_json {
            None => Ok(BTreeMap::new()),
            Some(bytes) => std::str::from_utf8(bytes).map_err(|_| "the file is not text".to_string()).and_then(parse_file).and_then(|entries| {
                if entries.len() > MAX_PROJECT_SERVERS {
                    return Err(format!("{} servers, at most {MAX_PROJECT_SERVERS} for a project", entries.len()));
                }
                for (name, e) in &entries {
                    if name.chars().count() > MAX_PROJECT_SERVER_NAME {
                        return Err(format!("server {name}: a name in a repository's pack is at most {MAX_PROJECT_SERVER_NAME} characters"));
                    }
                    if e.is_remote() {
                        return Err(format!("server {name}: a server of a repository is a program on this computer, not a remote one"));
                    }
                    if e.env.values().any(|v| matches!(v, EnvValue::Secret { .. })) {
                        return Err(format!("server {name}: a repository cannot ask for a secret of yours (the program is given what it needs by you, in your own mcp.json)"));
                    }
                    // It runs in its project: a folder of its own, if it names one, is a folder inside the project.
                    if let Some(cwd) = &e.cwd {
                        let unsafe_cwd = cwd.trim().is_empty() || cwd.starts_with(['/', '\\']) || cwd.contains(':') || cwd.split(['/', '\\']).any(|part| part == "..");
                        if unsafe_cwd {
                            return Err(format!("server {name}: a repository's server runs in its project folder, so its cwd is a folder inside it (no drive, no leading slash, no ..)"));
                        }
                    }
                }
                Ok(entries)
            }),
        };
        let (entries, problem) = match parsed {
            Ok(entries) => (entries, None),
            Err(why) => (BTreeMap::new(), Some(why)),
        };
        let wanted: BTreeMap<String, (String, ServerEntry)> = entries.into_iter().map(|(name, e)| (project_server_id(&key, &name), (name, e))).collect();
        let mut out = ProjectSync::default();
        // What is there now for this project.
        let mut gone: Vec<String> = Vec::new();
        {
            let servers = self.lock();
            for (id, slot) in servers.iter() {
                if slot.binding.as_ref().is_some_and(|b| b.key == key) {
                    match wanted.get(id) {
                        Some((_, entry)) if fingerprint(id, entry) == fingerprint(id, &slot.entry) => out.kept.push(id.clone()),
                        Some(_) => {
                            gone.push(id.clone());
                            out.changed.push(id.clone());
                        }
                        None => {
                            gone.push(id.clone());
                            out.removed.push(id.clone());
                        }
                    }
                }
            }
        }
        for id in &gone {
            self.drop_slot(id);
        }
        {
            let mut servers = self.lock();
            for (id, (name, entry)) in &wanted {
                if out.kept.contains(id) {
                    continue;
                }
                if servers.get(id).is_some_and(|s| s.binding.is_none()) {
                    tracing::warn!("MCP server {id} of the project {root} is not used: a server of yours has that name");
                    continue;
                }
                servers.insert(id.clone(), Slot { entry: entry.clone(), standing: Standing::NotTrusted, provider: None, binding: Some(Binding { key: key.clone(), shown: root.to_string(), name: name.clone() }) });
                if !out.changed.contains(id) {
                    out.added.push(id.clone());
                }
                out.to_start.push(id.clone());
            }
        }
        // Only the servers the person trusted are started, and only if they are as they were trusted: `start_servers` looks at that.
        match problem {
            Some(why) => Err(why),
            None => Ok(out),
        }
    }

    /// The folders of the projects that have servers here (as they were found), once each.
    pub fn project_roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self.lock().values().filter_map(|s| s.binding.as_ref().map(|b| b.shown.clone())).collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// Reads `mcp.json` again (card OA11-03, task EX-13) and makes the servers what it says **without ever trusting more than the person did**: a server that is gone from the file is stopped and
    /// its tools leave the host; a server whose entry changed is stopped and comes back not trusted; a new server is listed, not started. A server that is as it was is left running.
    /// A file that cannot be read stops every server of the person's (they are `Failed`, with why) and the answer is the error: a wrong file never keeps the old, looser state.
    pub fn reload(&self) -> Result<Reload, String> {
        let text = match std::fs::read_to_string(Self::config_path(&self.dir)) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(format!("unreadable: {e}")),
        };
        let parsed = text.and_then(|t| if t.trim().is_empty() { Ok(BTreeMap::new()) } else { parse_file(&t) });
        let entries = match parsed {
            Ok(entries) => entries,
            Err(why) => {
                let ids: Vec<String> = self.lock().iter().filter(|(_, s)| s.binding.is_none()).map(|(id, _)| id.clone()).collect();
                for id in &ids {
                    let provider = self.lock().get_mut(id).and_then(|s| {
                        s.standing = Standing::Failed(format!("mcp.json is wrong ({why}): no server of yours is used until it is fixed"));
                        s.provider.take()
                    });
                    if let Some(p) = provider {
                        p.disable();
                    }
                    self.host.unregister(&format!("mcp_{id}"));
                }
                return Err(why);
            }
        };
        let mut out = Reload::default();
        let current: Vec<(String, ServerEntry, Standing)> = self.lock().iter().filter(|(_, s)| s.binding.is_none()).map(|(id, s)| (id.clone(), s.entry.clone(), s.standing.clone())).collect();
        for (id, old, standing) in &current {
            match entries.get(id) {
                None => {
                    self.drop_slot(id);
                    out.removed.push(id.clone());
                }
                Some(new) if fingerprint(id, new) != fingerprint(id, old) => {
                    let provider = self.lock().get_mut(id).and_then(|s| {
                        s.entry = new.clone();
                        s.standing = Standing::NotTrusted;
                        s.provider.take()
                    });
                    if let Some(p) = provider {
                        p.disable();
                    }
                    self.host.unregister(&format!("mcp_{id}"));
                    out.changed.push(id.clone());
                }
                Some(new) => {
                    if let Some(s) = self.lock().get_mut(id) {
                        s.entry = new.clone();
                    }
                    // A server that a wrong file stopped comes back, when it is trusted as it is written.
                    if matches!(standing, Standing::Failed(why) if why.starts_with("mcp.json is wrong")) {
                        out.to_start.push(id.clone());
                    }
                    out.kept.push(id.clone());
                }
            }
        }
        for (id, entry) in entries {
            if current.iter().any(|(known, _, _)| *known == id) {
                continue;
            }
            if self.lock().get(&id).is_some() {
                continue;
            }
            self.lock().insert(id.clone(), Slot { entry, standing: Standing::NotTrusted, provider: None, binding: None });
            out.added.push(id);
        }
        Ok(out)
    }

    /// Start a server and put its tools in the host.
    fn bring_up(&self, id: &str, entry: &ServerEntry, accept: bool) -> Result<(), String> {
        let resolve = self.resolve.clone();
        let binding = self.lock().get(id).and_then(|s| s.binding.clone());
        // A server of a repository runs in its project's folder (or a folder inside it), not wherever the core happens to be. What the person trusted is the entry as it is written: this is
        // where it runs, which the list of servers shows as the project.
        let placed;
        let entry = match &binding {
            Some(b) => {
                let mut e = entry.clone();
                let root = std::path::PathBuf::from(&b.shown);
                e.cwd = Some(match &entry.cwd {
                    Some(inside) => root.join(inside).to_string_lossy().to_string(),
                    None => b.shown.clone(),
                });
                placed = e;
                &placed
            }
            None => entry,
        };
        // The secrets this server is given are masked before it is started, so that nothing it says in the first moment (an error that repeats a header, say) can be seen unmasked.
        for t in entry.env.values().chain(entry.headers.values()) {
            if let EnvValue::Secret { secret } = t {
                if let Some(v) = (resolve)(secret) {
                    self.host.register_secret(&v);
                }
            }
        }
        let connected = McpProvider::connect_with(id, entry, accept, &*resolve, &self.options_for(binding.as_ref())).map_err(|why| self.host.redact_text(&why))?;
        let provider = Arc::new(connected.provider);
        // Whatever was registered under this name before (a server that was stopped and is trusted again) goes first.
        self.host.unregister(&format!("mcp_{id}"));
        if let Err(e) = self.host.register(provider.clone()) {
            provider.disable();
            return Err(format!("its tools could not be registered: {e}"));
        }
        // The slot may have gone while the server started (its project's pack was untrusted, or the file changed): a server nobody lists is not left running.
        let mut servers = self.lock();
        match servers.get_mut(id) {
            Some(s) => {
                s.standing = Standing::Ready;
                s.provider = Some(provider);
            }
            None => {
                drop(servers);
                provider.disable();
                self.host.unregister(&format!("mcp_{id}"));
                return Err("it is not in the list of servers any more".into());
            }
        }
        Ok(())
    }

    /// The person trusts this server (as it is written now): it is started, its tools are recorded and put in the host, and the trust is
    /// kept for this entry.
    pub fn trust(&self, id: &str) -> Result<(), String> {
        let entry = self.lock().get(id).map(|s| s.entry.clone()).ok_or_else(|| format!("no server called {id} in mcp.json"))?;
        // The tag first: a record that is keyed on a machine with no key cannot take it, and the server is not started for nothing.
        let tag = self.tag_for(id, &entry, self.trusted_for_writing().version).ok_or_else(|| "the record of trust is keyed and this machine has no key for it, so nothing can be trusted until it has one".to_string())?;
        self.set_standing(id, Standing::Starting);
        if let Err(why) = self.bring_up(id, &entry, true) {
            self.set_standing(id, Standing::Failed(why.clone()));
            return Err(why);
        }
        let mut t = self.trusted_for_writing();
        t.servers.insert(id.to_string(), tag);
        self.save_trusted(&t)
    }

    /// The person stops trusting this server: its calls are refused, it is stopped and its tools leave the host.
    pub fn untrust(&self, id: &str) -> Result<(), String> {
        let provider = {
            let mut servers = self.lock();
            let slot = servers.get_mut(id).ok_or_else(|| format!("no server called {id} in mcp.json"))?;
            slot.standing = Standing::NotTrusted;
            slot.provider.take()
        };
        if let Some(p) = provider {
            p.disable();
        }
        self.host.unregister(&format!("mcp_{id}"));
        let mut t = self.trusted();
        t.servers.remove(id);
        self.save_trusted(&t)
    }

    pub fn views(&self) -> Vec<ServerView> {
        let trust = self.trusted();
        self.lock()
            .iter()
            .map(|(id, s)| {
                // A server that was given up on (it ended too often) says so where the person looks: it is `Failed`, with the reason.
                let standing = match (&s.standing, s.provider.as_ref().and_then(|p| p.failure())) {
                    (Standing::Ready, Some(why)) => Standing::Failed(why),
                    (other, _) => other.clone(),
                };
                let from_pack = s.binding.is_some();
                ServerView {
                    id: id.clone(),
                    command_line: command_line(&s.entry),
                    cwd: s.entry.cwd.as_ref().map(|c| aqua_tool_host::shown_whole(c, MAX_ENV_VALUE).unwrap_or_else(|n| format!("[{n} characters]"))),
                    env: s
                        .entry
                        .env
                        .iter()
                        .map(|(k, v)| match v {
                            EnvValue::Literal(value) => json!({"name": k, "value": aqua_tool_host::shown_whole(value, MAX_ENV_VALUE).unwrap_or_else(|n| format!("[{n} characters]"))}),
                            EnvValue::Secret { secret } => json!({"name": k, "secret": secret}),
                        })
                        .collect(),
                    tools_allow: s.entry.tools_allow.clone(),
                    read_only: if from_pack { Vec::new() } else { s.entry.read_only.clone() },
                    trusted: self.is_trusted(&trust, id, &s.entry),
                    standing,
                    tools: s.provider.as_ref().map(|p| p.tools().iter().map(|t| json!({"name": t.host_name, "description": t.description, "read_only": t.read_only})).collect()).unwrap_or_default(),
                    skipped: s.provider.as_ref().map(|p| p.skipped().to_vec()).unwrap_or_default(),
                    name: s.binding.as_ref().map_or_else(|| id.clone(), |b| b.name.clone()),
                    source: if from_pack { "project" } else { "user" },
                    project: s.binding.as_ref().map(|b| b.shown.clone()),
                    kind: if s.entry.is_remote() { "remote" } else { "program" },
                    url: s.entry.url.clone(),
                    header_names: s.entry.headers.keys().cloned().collect(),
                    expose: if from_pack { Expose::Named } else { s.entry.expose },
                    scope: s.entry.scope.clone(),
                    omitted_tools: s.provider.as_ref().map_or(0, |p| p.omitted_tools()),
                    quarantined: s.provider.as_ref().map(|p| p.quarantined()).unwrap_or_default(),
                    restarts: s.provider.as_ref().map_or(0, |p| p.restarts_used()),
                }
            })
            .collect()
    }

    /// The servers in a few numbers.
    pub fn summary(&self) -> Summary {
        Summary::of(&self.views())
    }

    /// How the tools of a server are shown and which runs it is for, by the id of the server in the host (`mcp_<id>.<tool>` has the id in it). `None` for an id that is not a server of
    /// the person's. A server from a repository is always `named` (its file cannot widen how it is shown) and its scope is its project.
    pub fn exposure_of(&self, id: &str) -> Option<Exposure> {
        self.lock().get(id).map(|s| match &s.binding {
            Some(b) => Exposure { name: b.name.clone(), expose: Expose::Named, scope: ScopeRule::of(&s.entry, Some(&b.key)) },
            None => Exposure { name: id.to_string(), expose: s.entry.expose, scope: ScopeRule::of(&s.entry, None) },
        })
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    /// RFC 4231, test cases 1, 2 and 6 (a key longer than a block): the HMAC that keys the record of trust is the standard one.
    #[test]
    fn two_tags_are_the_same_only_when_every_character_is() {
        assert!(super::same_tag("hmac:ab12", "hmac:ab12") && super::same_tag("", ""));
        assert!(!super::same_tag("hmac:ab12", "hmac:ab13") && !super::same_tag("hmac:ab12", "hmac:ab1") && !super::same_tag("hmac:ab12", "xmac:ab12"));
    }

    #[test]
    fn the_hmac_that_keys_the_trust_record_is_the_standard_one() {
        assert_eq!(hex_lower(&hmac_sha256(&[0x0b; 20], b"Hi There")), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        assert_eq!(hex_lower(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        assert_eq!(hex_lower(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    use super::*;

    fn good() -> &'static str {
        r#"{"mcpServers": {
            "notes": {"command": "npx.cmd", "args": ["-y", "some-notes-server", "D:/my notes"], "env": {"NOTES_DIR": "D:/notes", "NOTES_KEY": {"secret": "notes-key"}}, "tools_allow": ["add_note", "list_*"], "read_only": ["list_*"], "timeout_ms": 20000},
            "plain": {"command": "C:/tools/server.exe"}
        }}"#
    }

    #[test]
    fn the_file_is_read_in_the_common_notation_and_a_mistake_refuses_all_of_it() {
        let s = parse_file(good()).unwrap();
        assert_eq!(s.len(), 2);
        let n = &s["notes"];
        assert_eq!((n.command.as_str(), n.args.len(), n.timeout_ms, n.tools_allow.len(), n.read_only.len()), ("npx.cmd", 3, 20_000, 2, 1));
        assert_eq!(n.env["NOTES_KEY"], EnvValue::Secret { secret: "notes-key".into() });
        assert_eq!(n.env["NOTES_DIR"], EnvValue::Literal("D:/notes".into()));
        assert_eq!(s["plain"].timeout_ms, DEFAULT_TIMEOUT_MS);
        let bad = |entry: &str| parse_file(&format!(r#"{{"mcpServers": {{{entry}}}}}"#)).unwrap_err();
        assert!(bad(r#""Bad Name": {"command": "x"}"#).contains("a-z, 0-9 and _"));
        assert!(bad(r#""1abc": {"command": "x"}"#).contains("starting with a letter"));
        assert!(bad(r#""a": {"command": "  "}"#).contains("program name or path"));
        assert!(bad(r#""a": {"command": "x", "timeout_ms": 0}"#).contains("timeout_ms"));
        assert!(bad(r#""a": {"command": "x", "timeout_ms": 999999}"#).contains("timeout_ms"));
        assert!(bad(r#""a": {"command": "x", "env": {"A B": "1"}}"#).contains("bad name"));
        assert!(bad(r#""a": {"command": "x", "env": {"K": {"secret": ""}}}"#).contains("secret"));
        assert!(bad(r#""a": {"command": "x", "tools_allow": [""]}"#).contains("tool pattern"));
        assert!(bad(r#""a": {"command": "x", "trusted": true}"#).contains("not valid"), "a field that says the server is trusted is not accepted");
        for text in ["", "[]", "{}", r#"{"mcpServers": 1}"#, "nope"] {
            assert!(parse_file(text).is_err(), "{text:?}");
        }
        let many = (0..17).map(|i| format!(r#""s{i}": {{"command": "x"}}"#)).collect::<Vec<_>>().join(",");
        assert!(bad(&many).contains("at most 16"));
        assert!(parse_file(r#"{"mcpServers": {}}"#).unwrap().is_empty());
    }

    #[test]
    fn comments_and_trailing_commas_are_accepted_and_a_mistake_is_placed_in_the_persons_file() {
        // The notation other programs use is edited by hand: comments and a trailing comma are fine (task EX-02), and do not change what is read.
        let commented = "// the servers I use\n{\"mcpServers\": {\n  /* notes */\n  \"notes\": {\"command\": \"npx.cmd\", \"args\": [\"-y\", \"some-notes-server\",],},\n}}\n";
        let s = parse_file(commented).unwrap();
        assert_eq!((s.len(), s["notes"].args.len()), (1, 2));
        assert_eq!(parse_file("\u{feff}// x\n{\"mcpServers\": {}}").unwrap().len(), 0);
        // A missing comma on the fourth line is reported on the fourth line, although two comments are above it.
        let broken = "// one\n/* two\n   three */ {\"mcpServers\": {\"a\": {\"command\": \"x\"}\n  \"b\": {\"command\": \"y\"}}}";
        let said = parse_file(broken).unwrap_err();
        assert!(said.starts_with("line 4, column 3: ") || said.contains("at line 4 column 3"), "{said}");
        // A comment cannot hide a field this file does not accept.
        let said = parse_file("{\"mcpServers\": {\"a\": {\"command\": \"x\", /* \"trusted\": true, */ \"trusted\": true}}}").unwrap_err();
        assert!(said.contains("not valid") && said.contains("trusted"), "{said}");
        assert!(parse_file("{\"mcpServers\": {\"a\": {\"command\": \"x /* not a comment */\"}}}").unwrap()["a"].command.contains("/* not a comment */"), "a comment marker inside a string is text");
    }

    #[test]
    fn trust_is_for_this_command_line_these_variables_and_these_lists_and_not_for_a_secrets_value() {
        let s = parse_file(good()).unwrap();
        let base = fingerprint("notes", &s["notes"]);
        assert_eq!(base, fingerprint("notes", &s["notes"]));
        assert_ne!(base, fingerprint("other", &s["notes"]), "the name is part of it");
        for change in [
            |e: &mut ServerEntry| e.command = "other.cmd".into(),
            |e: &mut ServerEntry| e.args.push("--extra".into()),
            |e: &mut ServerEntry| e.cwd = Some("C:/x".into()),
            |e: &mut ServerEntry| {
                e.env.insert("NEW".into(), EnvValue::Literal("1".into()));
            },
            |e: &mut ServerEntry| e.tools_allow.push("more".into()),
            |e: &mut ServerEntry| e.read_only.push("add_note".into()),
        ] {
            let mut e = s["notes"].clone();
            change(&mut e);
            assert_ne!(fingerprint("notes", &e), base);
        }
        // The value of an ordinary variable is part of what was trusted (it can change what a program does as much as an argument); a secret is
        // trusted by its name, and its value, which is not in the file, never is.
        let mut e = s["notes"].clone();
        e.env.insert("NOTES_DIR".into(), EnvValue::Literal("D:/other".into()));
        assert_ne!(fingerprint("notes", &e), base, "a changed value is a different server");
        let mut e = s["notes"].clone();
        e.env.insert("NOTES_KEY".into(), EnvValue::Secret { secret: "another-secret".into() });
        assert_ne!(fingerprint("notes", &e), base, "another secret is a different server");
    }

    #[test]
    fn the_command_line_is_shown_whole_with_quotes_where_there_are_spaces_and_one_too_long_to_show_is_not_accepted() {
        let s = parse_file(good()).unwrap();
        assert_eq!(command_line(&s["notes"]), r#"npx.cmd -y some-notes-server "D:/my notes""#);
        // Just under the limit it is all there, the end included.
        let mut long = s["plain"].clone();
        long.args = vec!["a".repeat(700), "--exfiltrate=https://collector.example".into(), "b".repeat(600)];
        let shown = command_line(&long);
        assert!(shown.contains("--exfiltrate=https://collector.example") && shown.ends_with(&"b".repeat(600)) && !shown.contains('…'), "{shown}");
        // Over it the file is refused, whichever argument is long: nothing is hidden by cutting.
        for args in [format!(r#"["{}","{}"]"#, "a".repeat(1000), "b".repeat(600)), format!(r#"["{}","{}","x"]"#, "a".repeat(900), "b".repeat(900))] {
            let text = format!(r#"{{"mcpServers": {{"big": {{"command": "prog", "args": {args}}}}}}}"#);
            let said = parse_file(&text).unwrap_err();
            assert!(said.contains("the command line is") && said.contains("hides part of it"), "{said}");
        }
        // A line break or a direction override in an argument is not drawn as it is.
        let mut sneaky = s["plain"].clone();
        sneaky.args = vec!["x\nApproved: yes".into(), "evil\u{202e}txt.exe".into()];
        let shown = command_line(&sneaky);
        assert!(!shown.contains(['\n', '\u{202e}']) && shown.contains("U+202E"), "{shown:?}");
        // The values and the folder that the trust is for are shown whole too, or the file is refused.
        let text = format!(r#"{{"mcpServers": {{"v": {{"command": "prog", "env": {{"A": "{}"}}}}}}}}"#, "x".repeat(600));
        assert!(parse_file(&text).unwrap_err().contains("shown whole"));
    }

    #[test]
    fn tool_names_are_case_sensitive_so_a_pattern_for_one_tool_is_not_one_for_another() {
        let list = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(pattern_matches(&list(&["list_*", "add_note"]), "list_notes") && pattern_matches(&list(&["add_note"]), "add_note"));
        let not = |patterns: &[&str], name: &str| assert!(!pattern_matches(&list(patterns), name), "{patterns:?} must not name {name}");
        not(&["list_*"], "LIST_DELETE_ALL");
        not(&["list_*"], "List_notes");
        not(&["add_note"], "Add_Note");
        not(&["add_note"], "add_note2");
        not(&["Delete"], "delete");
        assert!(pattern_matches(&list(&["*"]), "anything") && !pattern_matches(&[], "x"));
    }

    #[test]
    fn a_bare_program_name_is_looked_for_in_path_with_the_windows_extensions() {
        let dir = std::env::temp_dir().join(format!("aqua_mcp_resolve_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("npx.cmd"), b"").unwrap();
        std::fs::write(dir.join("tool.exe"), b"").unwrap();
        let path = format!("C:/does/not/exist;{}", dir.display());
        let found = resolve_program("npx", &path, ".COM;.EXE;.BAT;.CMD");
        assert!(found.to_ascii_lowercase().ends_with("npx.cmd"), "{found}");
        assert!(resolve_program("tool.exe", &path, ".COM;.EXE").to_ascii_lowercase().ends_with("tool.exe"));
        assert_eq!(resolve_program("nothing_here", &path, ".EXE"), "nothing_here", "not found: left as it is, the start says so");
        assert_eq!(resolve_program("C:/x/y.exe", &path, ".EXE"), "C:/x/y.exe", "a path is used as it is");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tool_name_becomes_a_valid_host_name_and_two_that_collide_stay_two() {
        let is_valid = aqua_tool_host::is_valid_tool_name;
        for (wire, want) in [("echo", "mcp_notes.echo"), ("add-note", "mcp_notes.add_note"), ("Add Note!", "mcp_notes.add_note"), ("123go", "mcp_notes.t_123go"), ("--", "mcp_notes.tool"), ("a.b.c", "mcp_notes.a_b_c")] {
            let n = host_tool_name("notes", wire, &[]);
            assert_eq!(n, want, "{wire}");
            assert!(is_valid(&n), "{n}");
        }
        let first = host_tool_name("notes", "add-note", &[]);
        assert_eq!(host_tool_name("notes", "add_note", std::slice::from_ref(&first)), "mcp_notes.add_note_2");
        assert!(host_tool_name("notes", &"x".repeat(200), &[]).len() <= 50);
    }

    #[test]
    fn a_result_is_its_text_and_a_note_for_what_is_not_text() {
        let r = json!({"content": [{"type": "text", "text": "note one"}, {"type": "image", "data": "AAAAAAAA", "mimeType": "image/png"}, {"type": "resource", "resource": {"uri": "file:///n/two.txt"}}, {"type": "weird<>"}]});
        let t = render_result(&r);
        assert!(t.starts_with("note one\n[an image of 6 bytes was returned and is not shown]\n[a resource: file:///n/two.txt]\n[a part of type weird]"), "{t}");
        assert_eq!(render_result(&json!({"content": []})), "Done.");
        assert_eq!(render_result(&json!({"structuredContent": {"a": 1}})), r#"{"a":1}"#);
    }

    fn one(entry: &str) -> Result<ServerEntry, String> {
        parse_file(&format!(r#"{{"mcpServers": {{"srv": {entry}}}}}"#)).map(|mut m| m.remove("srv").unwrap())
    }

    fn run(project: Option<&str>, agent: Option<&str>) -> RunInfo {
        RunInfo { project: project.map(str::to_string), agent: agent.map(str::to_string) }
    }

    /// Card OA11-03: an entry with none of the new fields has the fingerprint it always had, so that nobody who trusted a server has to trust it again; with one of them it has another.
    #[test]
    fn the_fingerprint_of_an_entry_without_the_new_fields_is_what_it_always_was_and_the_new_fields_change_it() {
        let e = one(r#"{"command": "npx.cmd", "args": ["-y", "x"], "env": {"A": "1", "K": {"secret": "k"}}, "cwd": "D:/w", "tools_allow": ["a*"], "read_only": ["a1"]}"#).unwrap();
        // The text that made the fingerprint before the card: the same keys, written the same way.
        let env: Vec<(&String, String)> = e.env.iter().map(|(k, v)| (k, match v { EnvValue::Secret { secret } => format!("secret:{secret}"), EnvValue::Literal(s) => format!("value:{s}") })).collect();
        let old = json!({"id": "srv", "command": e.command.trim(), "args": e.args, "cwd": e.cwd, "env": env, "tools_allow": e.tools_allow, "read_only": e.read_only});
        let old_hash: String = Sha256::digest(old.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(fingerprint("srv", &e), old_hash, "the record of what the person trusted still matches");
        let base = fingerprint("srv", &e);
        for change in [
            |e: &mut ServerEntry| e.expose = Expose::Always,
            |e: &mut ServerEntry| e.expose = Expose::Ranked,
            |e: &mut ServerEntry| e.scope.projects.push("e:/work/*".into()),
            |e: &mut ServerEntry| e.scope.agents.push("build".into()),
        ] {
            let mut changed = e.clone();
            change(&mut changed);
            assert_ne!(fingerprint("srv", &changed), base);
        }
        let remote = one(r#"{"url": "https://mcp.example.com/mcp", "headers": {"Authorization": {"secret": "tok"}}}"#).unwrap();
        let other_url = one(r#"{"url": "https://mcp.example.org/mcp", "headers": {"Authorization": {"secret": "tok"}}}"#).unwrap();
        let other_secret = one(r#"{"url": "https://mcp.example.com/mcp", "headers": {"Authorization": {"secret": "tok2"}}}"#).unwrap();
        assert_ne!(fingerprint("srv", &remote), fingerprint("srv", &other_url), "the address is part of what was trusted");
        assert_ne!(fingerprint("srv", &remote), fingerprint("srv", &other_secret), "so is the secret that is sent (by its name)");
    }

    #[test]
    fn the_new_fields_are_read_and_a_mistake_in_them_refuses_the_file() {
        let e = one(r#"{"command": "x", "expose": "ranked", "scope": {"projects": ["E:\\work\\*"], "agents": ["build", "review"]}}"#).unwrap();
        assert_eq!((e.expose, e.scope.projects.clone(), e.scope.agents.clone()), (Expose::Ranked, vec!["E:\\work\\*".to_string()], vec!["build".to_string(), "review".to_string()]));
        assert_eq!(one(r#"{"command": "x"}"#).unwrap().expose, Expose::Named, "the default is what it always was");
        let bad = |entry: &str| one(entry).unwrap_err();
        assert!(bad(r#"{"command": "x", "expose": "everywhere"}"#).contains("not valid"));
        assert!(bad(r#"{"command": "x", "scope": {"folders": []}}"#).contains("not valid"), "an unknown key in a scope is refused, not ignored");
        assert!(bad(r#"{"command": "x", "scope": {"agents": ["Build Agent"]}}"#).contains("agent in the scope"));
        assert!(bad(r#"{"command": "x", "scope": {"projects": [""]}}"#).contains("project in the scope"));
        let many = (0..17).map(|i| format!("\"e:/p{i}\"")).collect::<Vec<_>>().join(",");
        assert!(bad(&format!(r#"{{"command": "x", "scope": {{"projects": [{many}]}}}}"#)).contains("at most 16"));
        // The features of the audit that are not built are not accepted either: a field that does nothing would be a promise.
        assert!(bad(r#"{"command": "x", "features": ["prompts"]}"#).contains("not valid"));
    }

    #[test]
    fn a_remote_entry_is_an_address_with_headers_and_nothing_of_a_program_and_never_a_key_in_the_file() {
        let e = one(r#"{"url": "https://mcp.example.com/mcp", "headers": {"Authorization": {"secret": "tok"}, "X-Team": "red"}, "timeout_ms": 5000}"#).unwrap();
        assert!(e.is_remote() && e.command.is_empty());
        assert_eq!(command_line(&e), "remote server https://mcp.example.com/mcp (sends the headers Authorization, X-Team)");
        assert!(one(r#"{"url": "http://127.0.0.1:8123/mcp"}"#).is_ok() && one(r#"{"url": "http://localhost/mcp"}"#).is_ok(), "plain http to this computer only");
        let bad = |entry: &str| one(entry).unwrap_err();
        assert!(bad(r#"{"url": "http://mcp.example.com/mcp"}"#).contains("this computer only"));
        assert!(bad(r#"{"url": "ftp://mcp.example.com/mcp"}"#).contains("scheme"));
        assert!(bad(r#"{"url": "https://me:pw@mcp.example.com/mcp"}"#).contains("user name or password")); // secret-scan: ignore (a made-up login, to see it refused)
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp?key=abc123"}"#).contains("query"));
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp#x"}"#).contains("fragment"));
        assert!(bad(r#"{"url": "https://mcp.example.com/a b"}"#).contains("space"));
        assert!(bad(r#"{"url": "not a url"}"#).contains("space") || bad(r#"{"url": "not a url"}"#).contains("web address"));
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp", "command": "x"}"#).contains("none of command"));
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp", "args": ["a"]}"#).contains("none of command"));
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp", "env": {"A": "1"}}"#).contains("none of command"));
        assert!(bad(r#"{"command": "x", "headers": {"A": "1"}}"#).contains("belong to a remote server"));
        // A header that carries a credential is a secret by name, whatever it is called.
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp", "headers": {"Authorization": "Bearer abc"}}"#).contains("carries a credential"));
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp", "headers": {"X-Api-Token": "abc"}}"#).contains("carries a credential"));
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp", "headers": {"Cookie": "a=b"}}"#).contains("carries a credential"));
        // The protocol's own headers and bad names are not set from a file.
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp", "headers": {"Mcp-Session-Id": "1"}}"#).contains("protocol's own"));
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp", "headers": {"Host": "evil"}}"#).contains("protocol's own"));
        assert!(bad(r#"{"url": "https://mcp.example.com/mcp", "headers": {"A: b\r\nX": "1"}}"#).contains("not a header name"));
    }

    #[test]
    fn a_scope_names_projects_and_agents_in_any_spelling_and_only_narrows() {
        let open = ScopeRule::of(&one(r#"{"command": "x"}"#).unwrap(), None);
        assert!(open.is_open() && open.allows(&run(None, None)).is_ok(), "no scope: every run, and a call that is in no run");
        let e = one(r#"{"command": "x", "scope": {"projects": ["E:\\Work\\*", "d:/solo/app"], "agents": ["build"]}}"#).unwrap();
        let rule = ScopeRule::of(&e, None);
        assert!(!rule.is_open());
        // The spelling of the path does not matter (case, slashes, a trailing one, the \\?\ form of a path).
        for ok in ["E:\\work\\app", "e:/WORK/app/", "\\\\?\\E:\\Work\\other", "D:\\Solo\\App", "d:/solo/app/sub"] {
            assert!(rule.allows(&run(Some(ok), None)).is_ok(), "{ok}");
        }
        for no in ["E:\\elsewhere\\app", "d:/solo/application", "D:\\solo"] {
            assert!(rule.allows(&run(Some(no), None)).is_err(), "{no}");
        }
        assert!(rule.allows(&run(None, None)).unwrap_err().contains("some projects only"), "a call in no project is outside a scope");
        // An agent: a plain run is `build`; another agent is refused.
        assert!(rule.allows(&run(Some("e:/work/app"), Some("BUILD"))).is_ok());
        assert!(rule.allows(&run(Some("e:/work/app"), Some("plan"))).unwrap_err().contains("this run is plan"));
        let only_plan = ScopeRule::of(&one(r#"{"command": "x", "scope": {"agents": ["plan"]}}"#).unwrap(), None);
        assert!(only_plan.allows(&run(Some("e:/anywhere"), Some("plan"))).is_ok() && only_plan.allows(&run(None, None)).is_err(), "a plain run is build, so plan's server is not its own");
        // A server from a pack is for its project (and inside it) whatever its own scope says; its agents still narrow it.
        let packed = ScopeRule::of(&one(r#"{"command": "x", "scope": {"projects": ["z:/anywhere"], "agents": ["review"]}}"#).unwrap(), Some("E:\\Work\\App"));
        assert!(packed.allows(&run(Some("e:/work/app"), Some("review"))).is_ok() && packed.allows(&run(Some("e:/work/app/src"), Some("review"))).is_ok());
        assert!(packed.allows(&run(Some("z:/anywhere"), Some("review"))).unwrap_err().contains("another project"), "its own `projects` is not read");
        assert!(packed.allows(&run(Some("e:/work/app"), None)).is_err(), "and the agents narrow it further");
        assert!(packed.allows(&run(Some("e:/work/application"), Some("review"))).is_err(), "a folder that only begins with the name is another folder");
        assert!(!packed.is_open());
    }

    #[test]
    fn a_project_server_has_an_id_of_its_own_that_two_projects_do_not_share() {
        let a = project_server_id(&normalise_path("E:\\Work\\App"), "gh");
        let b = project_server_id(&normalise_path("E:\\Work\\Other"), "gh");
        assert_ne!(a, b);
        assert_eq!(a, project_server_id(&normalise_path("e:/work/app/"), "gh"), "the same folder, spelt another way, is the same id");
        assert!(valid_id(&a) && a.len() <= 24 && a.ends_with("_gh") && a.starts_with('p'), "{a}");
        assert!(valid_id(&project_server_id("x", &"n".repeat(MAX_PROJECT_SERVER_NAME))));
    }

    #[test]
    fn a_path_pattern_is_a_folder_or_has_stars_for_any_run_of_characters() {
        assert!(glob_fits("e:/work", "e:/work") && glob_fits("e:/work", "e:/work/app") && !glob_fits("e:/work", "e:/workshop"));
        assert!(glob_fits("e:/work/*", "e:/work/app") && glob_fits("e:/*/app", "e:/work/app") && glob_fits("*", "anything") && glob_fits("e:/w*k/*", "e:/work/x"));
        assert!(!glob_fits("e:/work/*", "e:/work") && !glob_fits("e:/*/app", "e:/work/apps") && !glob_fits("e:/work/*", "d:/work/app"));
        assert!(glob_fits("a*b*c", "aXXbYYc") && !glob_fits("a*b*c", "aXXbYY") && glob_fits("**", ""), "stars match runs, also empty ones");
        assert_eq!(normalise_path("  \\\\?\\E:\\Work\\App\\ "), "e:/work/app");
        assert_eq!(normalise_path("E:\\"), "e:/", "a drive root keeps its slash");
        assert_eq!(normalise_path("/"), "/");
    }
}
