//! HTTP tools: `http.request`, `web.fetch`, `web.read`, `web.search` and `web.scrape`.
//!
//! Every request passes the network policy (see [`policy`]); credentials are named auth profiles
//! whose secrets come from the credential store and go only to the hosts the profile lists;
//! responses are size-limited, cookies are never kept or returned, and everything a server sends
//! is labelled untrusted by the host.

pub mod client;
pub mod html;
pub mod policy;
pub mod read;
pub mod search;
pub mod sensitive;

use crate::fs::guard::{display, Op, PathGuard};
use crate::secrets::Secrets;
use aqua_tool_host::output::new_nonce;
use aqua_tool_host::{
    Annotations, Artifact, Availability, ChangePreview, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use base64::Engine;
use client::{Client, Credential, Request, Response};
pub use policy::{AuthKind, AuthProfile, HttpConfig, NetPolicy, SearchConfig, SearchEngine, Verdict};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const MAX_BODY_BYTES: usize = 1 << 20;
const MAX_UPLOAD_BYTES: u64 = 20 << 20;
const MAX_DOWNLOAD_BYTES: usize = 50 << 20;

struct Inner {
    client: Client,
    guard: Arc<PathGuard>,
    secrets: Arc<dyn Secrets>,
    cfg: HttpConfig,
    /// Searches made a moment ago (see [`search::Cache`]).
    searches: search::Cache,
}

/// One service to ask, with the request that asks it.
struct SearchPlan {
    engine: SearchEngine,
    url: String,
    profile: Option<String>,
    /// A JSON body (the service wants a POST).
    body: Option<Value>,
}

impl SearchEngine {
    fn name(self) -> &'static str {
        match self {
            SearchEngine::Searxng => "searxng",
            SearchEngine::Brave => "brave",
            SearchEngine::Tavily => "tavily",
            SearchEngine::Wikipedia => "wikipedia",
            SearchEngine::DuckDuckGo => "duckduckgo",
            SearchEngine::Parallel => "parallel",
            SearchEngine::Ollama => "ollama",
        }
    }
}

/// Is DuckDuckGo's results page asked when the person has set no service (or only some)? On unless `AQUA_SEARCH_DUCKDUCKGO=off`.
fn duckduckgo_enabled() -> bool {
    !matches!(std::env::var("AQUA_SEARCH_DUCKDUCKGO").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("off" | "0" | "false" | "no"))
}

/// The HTTP and web tools. State is behind an `Arc` so blocking tasks can own it.
pub struct HttpProvider(Arc<Inner>);

impl HttpProvider {
    pub fn new(cfg: HttpConfig, guard: Arc<PathGuard>, secrets: Arc<dyn Secrets>) -> Result<Self, String> {
        cfg.validate()?;
        let client = Client::new(Arc::new(NetPolicy::new(cfg.clone())));
        Ok(Self(Arc::new(Inner { client, guard, secrets, cfg, searches: search::Cache::default() })))
    }
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, msg)
}

fn arg_str<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn is_idempotent(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "PUT" | "DELETE" | "OPTIONS")
}

/// Headers the model may not set: hop-by-hop and framing headers, and anything that carries a credential
/// (those come from an auth profile, so a secret never has to be typed into a prompt).
fn check_header(name: &str, value: &str) -> Result<(), ToolError> {
    if name.is_empty() || name.len() > 100 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(bad(format!("'{name}' is not a valid header name")));
    }
    let l = name.to_ascii_lowercase();
    if ["host", "content-length", "transfer-encoding", "connection", "upgrade", "te", "trailer", "expect", "keep-alive", "proxy-connection", "accept-encoding"].contains(&l.as_str()) || l.starts_with("x-forwarded") || l.starts_with("sec-") {
        return Err(bad(format!("the '{name}' header is set by Aqua and cannot be overridden")));
    }
    if ["authorization", "proxy-authorization", "cookie", "set-cookie"].contains(&l.as_str()) || ["token", "secret", "apikey", "api-key", "auth", "password", "session"].iter().any(|w| l.contains(w)) {
        return Err(ToolError::new(ErrorCode::SecretBlocked, format!("the '{name}' header looks like a credential. Use an auth profile (the 'auth' argument) so the secret never passes through the model.")));
    }
    if value.len() > 2000 || value.chars().any(|c| c.is_control()) {
        return Err(bad(format!("the value of '{name}' is too long or contains control characters")));
    }
    Ok(())
}

fn spec(name: &str, description: &str, classes: Vec<PermClass>, props: Value, required: &[&str], read_only: bool, default_ms: u64, max_ms: u64) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        classes,
        default_decision: Decision::Auto,
        input_schema: json!({"type": "object", "properties": props, "required": required}),
        timeout_default_ms: default_ms,
        timeout_max_ms: max_ms,
        annotations: Annotations { read_only, destructive: false, idempotent: read_only, open_world: true },
        untrusted_output: true,
    }
}

impl ToolProvider for HttpProvider {
    fn id(&self) -> &str {
        "http"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let url = json!({"type": "string", "minLength": 1, "maxLength": 2048});
        vec![
            spec("http.request", "Send an HTTP request and return status, headers and body. Credentials come from a named auth profile (auth), never from the arguments.", vec![PermClass::Network],
                json!({
                    "method": {"type": "string", "enum": ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"], "default": "GET"},
                    "url": url.clone(),
                    "headers": {"type": "object", "additionalProperties": true},
                    "query": {"type": "object", "additionalProperties": true},
                    "json": {},
                    "body": {"type": "string", "maxLength": 1048576},
                    "form": {"type": "object", "additionalProperties": true},
                    "multipart": {"type": "object", "additionalProperties": true},
                    "auth": {"type": "string", "maxLength": 40},
                    "timeout_s": {"type": "integer", "minimum": 1, "maximum": 120, "default": 30},
                    "max_response_kb": {"type": "integer", "minimum": 1, "maximum": 2048, "default": 256},
                    "follow_redirects": {"type": "boolean", "default": true},
                    "retries": {"type": "integer", "minimum": 0, "maximum": 3, "default": 0},
                    "save_to": {"type": "string", "maxLength": 1024},
                    "overwrite": {"type": "boolean", "default": false}
                }), &["url"], false, 150_000, 300_000),
            spec("web.fetch", "Fetch a web page and return its readable text (scripts, styles and hidden text removed).", vec![PermClass::Network],
                json!({"url": url, "max_chars": {"type": "integer", "minimum": 500, "maximum": 20000, "default": 6000}}), &["url"], true, 60_000, 120_000),
            spec("web.search", "Search the web: a numbered list [n] domain, date: snippet, with each address. Cite results as [n]. Wikipedia is the fallback.", vec![PermClass::Network],
                json!({"query": {"type": "string", "minLength": 1, "maxLength": 300}, "limit": {"type": "integer", "minimum": 1, "maximum": 10, "default": 8}}), &["query"], true, 45_000, 90_000),
            spec("web.read", "Read the part of a page that answers goal: its main text (about 600 tokens), title, date and links. Use it on a result of web.search.", vec![PermClass::Network],
                json!({"url": url.clone(), "goal": {"type": "string", "maxLength": 300}, "max_chars": {"type": "integer", "minimum": 500, "maximum": 8000, "default": 2400}}), &["url"], true, 60_000, 120_000),
            spec("web.scrape", "Advanced scraping of a web page using Scrapling (supports JS execution, CSS selectors, stealth).", vec![PermClass::Network],
                json!({"url": url, "css_selector": {"type": "string", "default": ""}, "stealth": {"type": "boolean", "default": true}}), &["url"], true, 90_000, 180_000),
        ]
    }

    fn availability(&self, tool: &str) -> Availability {
        if tool == "web.scrape" && !self.0.cfg.scrape {
            return Availability::Unavailable("web scraping is switched off (\"scrape\": false in data/tools/http.json)".into());
        }
        Availability::Available
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let s = &self.0;
        match tool {
            "web.search" => {
                // The services are the user's own configuration (and Wikipedia, which needs no key), so they are trusted. The search may fall
                // back from one to the next, so the call is judged by all the services it may use: every host it may reach is named, and it
                // counts as using a credential when any of them carries one.
                let plans = s.search_plan(arg_str(args, "query"), 1)?;
                let mut hosts: Vec<String> = Vec::new();
                let mut with_key = false;
                for p in &plans {
                    if let Ok(host) = s.search_service_ok(p) {
                        if !hosts.contains(&host) {
                            hosts.push(host);
                        }
                        with_key |= p.profile.is_some();
                    }
                }
                if hosts.is_empty() {
                    return Err(ToolError::new(ErrorCode::NetworkBlocked, "the network policy refuses every search service that is set up (or a service's credential may not be sent to it)"));
                }
                let mut classes = vec![PermClass::Network];
                if with_key {
                    classes.push(PermClass::Credential);
                }
                Ok(Classification { classes, target: hosts.join(", "), summary: format!("Search the web for \"{}\"", truncate(arg_str(args, "query"), 100)), target_allowed: true })
            }
            "web.fetch" | "web.read" | "http.request" | "web.scrape" => {
                let raw = arg_str(args, "url");
                let mut url = raw.to_string();
                if tool == "http.request" {
                    url = s.build_url(args)?;
                }
                let t = NetPolicy::parse(&url).map_err(|e| ToolError::new(ErrorCode::NetworkBlocked, e))?;
                let verdict = s.client.policy().check(&t);
                if let Verdict::Deny(why) = &verdict {
                    return Err(ToolError::new(ErrorCode::NetworkBlocked, why.clone()));
                }
                let mut classes = vec![PermClass::Network];
                let method = if matches!(tool, "web.fetch" | "web.read" | "web.scrape") { "GET".to_string() } else { s.method(args)? };
                let mut summary = format!("{method} {}", truncate(&url, 300));
                // Where a download is saved is judged like any file write: only a trusted folder runs without asking.
                let mut write_trusted = true;
                if tool == "http.request" {
                    s.validate_request(args)?;
                    if !matches!(method.as_str(), "GET" | "HEAD") {
                        // Posting, putting, patching or deleting affects someone else's system.
                        classes.push(PermClass::ExternalEffect);
                    }
                    if let Some(name) = args.get("auth").and_then(|v| v.as_str()).filter(|n| !n.is_empty()) {
                        let p = s.cfg.auth.get(name).ok_or_else(|| ToolError::new(ErrorCode::AuthRequired, format!("no auth profile '{name}'. Profiles: {}", s.cfg.auth.keys().cloned().collect::<Vec<_>>().join(", "))))?;
                        if !s.client.policy().profile_allows(p, &t.host, t.port) {
                            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("the '{name}' credential may not be sent to {}", t.host)));
                        }
                        if !NetPolicy::credential_transport_ok(&t) {
                            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("the '{name}' credential is only sent over https (this address is plain http)")));
                        }
                        classes.push(PermClass::Credential);
                        summary.push_str(&format!(" with the '{name}' credential"));
                    }
                    if let Some(files) = args.get("multipart").and_then(|m| m.get("files")).and_then(|f| f.as_array()) {
                        for f in files {
                            let r = s.guard.resolve(f.get("path").and_then(|p| p.as_str()).unwrap_or(""), Op::Read)?;
                            if r.sensitive && !classes.contains(&PermClass::Credential) {
                                classes.push(PermClass::Credential);
                            }
                            summary.push_str(&format!("; uploading {}", display(&r.path)));
                        }
                    }
                    if let Some(p) = args.get("save_to").and_then(|v| v.as_str()).filter(|p| !p.is_empty()) {
                        let r = s.guard.resolve(p, Op::Write)?;
                        classes.push(PermClass::Write);
                        write_trusted = r.trusted;
                        // A script, a startup entry, an instruction file (CLAUDE.md…) or a credential file is not "just a download".
                        if r.executable_kind {
                            classes.push(PermClass::Execute);
                        }
                        if r.sensitive && !classes.contains(&PermClass::Credential) {
                            classes.push(PermClass::Credential);
                        }
                        summary.push_str(&format!("; saving to {}", display(&r.path)));
                    }
                }
                Ok(Classification { classes, target: format!("{}:{}", t.host, t.port), summary, target_allowed: verdict == Verdict::Allow && write_trusted })
            }
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }

    /// What `http.request` would send, for the question (card OA6-04, SE-03): the whole address, the names of the headers, and the body. The question's own sentence cuts the address at
    /// 300 characters and says nothing of the body, which is what a post sends out.
    fn approval_preview(&self, tool: &str, args: &Value) -> Option<ChangePreview> {
        if tool != "http.request" {
            return None;
        }
        self.0.request_preview(args).map(ChangePreview::new)
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let inner = self.0.clone();
        let cancel = ctx.cancel.clone();
        let tool = tool.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || match tool.as_str() {
                "http.request" => inner.request(&args, &cancel),
                "web.fetch" => inner.fetch(&args, &cancel),
                "web.read" => inner.read(&args, &cancel),
                "web.search" => inner.search(&args, &cancel),
                "web.scrape" => inner.scrape(&args, &cancel),
                other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("http task failed: {e}")))?
        })
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// How much of a body a question shows, and of an address.
const PREVIEW_BODY_CHARS: usize = 800;
const PREVIEW_URL_CHARS: usize = 2000;
/// The mark before the number of characters a preview leaves out.
const HELLIP: &str = "\u{2026} ";

/// The first `max` characters of `text`, and how many more there are.
fn head_of(text: &str, max: usize) -> (String, usize) {
    let n = text.chars().count();
    (text.chars().take(max).collect(), n.saturating_sub(max))
}

impl Inner {
    /// The text of the preview of an `http.request` (see [`HttpProvider::approval_preview`]). `None` for a plain read (GET or HEAD, no body, no header, an address that the question's own
    /// sentence shows whole): there is nothing more to show.
    fn request_preview(&self, a: &Value) -> Option<String> {
        let method = self.method(a).ok()?;
        let url = self.build_url(a).ok()?;
        let headers: Vec<&str> = a.get("headers").and_then(|v| v.as_object()).map(|h| h.keys().map(|k| k.as_str()).collect()).unwrap_or_default();
        // The body as it is sent: the bytes `request` builds, for the kinds that are plain text. An upload is shown by its fields and the names and sizes of its files: its bytes are the files'
        // and its boundary is made when it is sent.
        let plain_body = a.get("json").is_some_and(|v| !v.is_null()) || a.get("body").is_some_and(|v| v.is_string()) || a.get("form").is_some_and(|v| v.is_object());
        let mut unused: Vec<(String, String)> = Vec::new();
        let (kind, bytes, files): (&str, Option<Vec<u8>>, Vec<String>) = if plain_body {
            let kind = if a.get("json").is_some_and(|v| !v.is_null()) { "JSON" } else if a.get("body").is_some_and(|v| v.is_string()) { "text" } else { "form" };
            (kind, self.body(a, &mut unused).ok().flatten(), Vec::new())
        } else if let Some(m) = a.get("multipart").filter(|v| v.is_object()) {
            let fields: Vec<String> = m.get("fields").and_then(|v| v.as_object()).into_iter().flatten().map(|(k, v)| format!("{k}={}", match v { Value::String(s) => s.clone(), other => other.to_string() })).collect();
            let files: Vec<String> = m
                .get("files")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .map(|f| {
                    let path = f.get("path").and_then(|p| p.as_str()).unwrap_or("");
                    match self.guard.resolve(path, Op::Read) {
                        Ok(r) => format!("file {} ({} bytes)", display(&r.path), std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0)),
                        Err(_) => format!("file {path}"),
                    }
                })
                .collect();
            ("multipart form", Some(fields.join("\n").into_bytes()), files)
        } else {
            ("", None, Vec::new())
        };
        let (url_head, url_more) = head_of(&url, PREVIEW_URL_CHARS);
        if bytes.is_none() && headers.is_empty() && matches!(method.as_str(), "GET" | "HEAD") && url_more == 0 && url.chars().count() <= 300 {
            return None;
        }
        let mut out = format!("{method} {url_head}");
        if url_more > 0 {
            out.push_str(&format!("\n{HELLIP}{url_more} more characters of the address are not shown"));
        }
        if !headers.is_empty() {
            out.push_str(&format!("\nHeaders: {}", headers.join(", ")));
        }
        if let Some(bytes) = bytes {
            let text = String::from_utf8_lossy(&bytes).to_string();
            let (shown, more) = head_of(&text, PREVIEW_BODY_CHARS);
            if files.is_empty() && kind != "multipart form" {
                let sha: String = Sha256::digest(&bytes).iter().take(8).map(|b| format!("{b:02x}")).collect();
                out.push_str(&format!("\nBody ({kind}, {} bytes, sha256 {sha}):", bytes.len()));
            } else {
                out.push_str(&format!("\nBody ({kind}):"));
            }
            if !shown.is_empty() {
                out.push_str(&format!("\n{shown}"));
            }
            if more > 0 {
                out.push_str(&format!("\n{HELLIP}{more} more characters not shown"));
            }
            for file in files {
                out.push_str(&format!("\n{file}"));
            }
        }
        Some(out)
    }

    fn method(&self, a: &Value) -> Result<String, ToolError> {
        let m = match arg_str(a, "method") {
            "" => "GET",
            m => m,
        };
        if !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"].contains(&m) {
            return Err(bad(format!("method '{m}' is not supported")));
        }
        Ok(m.to_string())
    }

    /// The URL with `query` added.
    fn build_url(&self, a: &Value) -> Result<String, ToolError> {
        let mut url = url::Url::parse(arg_str(a, "url").trim()).map_err(|e| ToolError::new(ErrorCode::NetworkBlocked, format!("not a valid URL: {e}")))?;
        if let Some(q) = a.get("query").and_then(|v| v.as_object()) {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in q {
                let text = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => continue,
                    other => other.to_string(),
                };
                pairs.append_pair(k, &text);
            }
        }
        Ok(url.to_string())
    }

    fn validate_request(&self, a: &Value) -> Result<(), ToolError> {
        let bodies = ["json", "body", "form", "multipart"].iter().filter(|k| a.get(**k).map(|v| !v.is_null()).unwrap_or(false)).count();
        if bodies > 1 {
            return Err(bad("give at most one of json, body, form and multipart"));
        }
        if bodies == 1 && matches!(self.method(a)?.as_str(), "GET" | "HEAD") {
            return Err(bad("GET and HEAD requests do not carry a body; use POST, PUT or PATCH"));
        }
        if let Some(h) = a.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in h {
                check_header(k, v.as_str().ok_or_else(|| bad(format!("header '{k}' must be a string")))?)?;
            }
        }
        if let Some(m) = a.get("multipart") {
            if !m.is_null() && !m.is_object() {
                return Err(bad("multipart must be an object with fields and files"));
            }
        }
        Ok(())
    }

    fn body(&self, a: &Value, headers: &mut Vec<(String, String)>) -> Result<Option<Vec<u8>>, ToolError> {
        let set_ct = |headers: &mut Vec<(String, String)>, ct: &str| {
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                headers.push(("Content-Type".into(), ct.into()));
            }
        };
        let check = |b: Vec<u8>| -> Result<Vec<u8>, ToolError> {
            if b.len() > MAX_BODY_BYTES {
                Err(ToolError::new(ErrorCode::ResourceLimit, format!("the request body is larger than {} bytes", MAX_BODY_BYTES)))
            } else {
                Ok(b)
            }
        };
        if let Some(j) = a.get("json").filter(|v| !v.is_null()) {
            set_ct(headers, "application/json");
            return Ok(Some(check(serde_json::to_vec(j).map_err(|e| bad(e.to_string()))?)?));
        }
        if let Some(b) = a.get("body").and_then(|v| v.as_str()) {
            set_ct(headers, "text/plain; charset=utf-8");
            return Ok(Some(check(b.as_bytes().to_vec())?));
        }
        if let Some(f) = a.get("form").and_then(|v| v.as_object()) {
            let mut ser = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in f {
                ser.append_pair(k, &match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
            }
            set_ct(headers, "application/x-www-form-urlencoded");
            return Ok(Some(check(ser.finish().into_bytes())?));
        }
        if let Some(m) = a.get("multipart").filter(|v| v.is_object()) {
            let boundary = format!("----aqua{}{}", new_nonce(), new_nonce());
            let mut out: Vec<u8> = Vec::new();
            if let Some(fields) = m.get("fields").and_then(|v| v.as_object()) {
                for (k, v) in fields {
                    if k.contains(['"', '\r', '\n']) {
                        return Err(bad(format!("field name '{k}' is not valid")));
                    }
                    let val = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    out.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{val}\r\n").as_bytes());
                }
            }
            let mut total = out.len() as u64;
            for f in m.get("files").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                let field = f.get("name").and_then(|v| v.as_str()).unwrap_or("file");
                let r = self.guard.resolve(f.get("path").and_then(|p| p.as_str()).unwrap_or(""), Op::Read)?;
                let meta = std::fs::metadata(&r.path).map_err(|e| ToolError::new(ErrorCode::NotFound, format!("{}: {e}", display(&r.path))))?;
                if !meta.is_file() {
                    return Err(bad(format!("{} is not a file", display(&r.path))));
                }
                total += meta.len();
                if total > MAX_UPLOAD_BYTES {
                    return Err(ToolError::new(ErrorCode::ResourceLimit, format!("the upload is larger than {} bytes", MAX_UPLOAD_BYTES)));
                }
                let bytes = std::fs::read(&r.path).map_err(|e| ToolError::new(ErrorCode::OsPermissionDenied, format!("{}: {e}", display(&r.path))))?;
                let filename = f.get("filename").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into()));
                if field.contains(['"', '\r', '\n']) || filename.contains(['"', '\r', '\n', '/', '\\']) {
                    return Err(bad("field and file names may not contain quotes, slashes or line breaks"));
                }
                out.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
                out.extend_from_slice(&bytes);
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
            set_ct(headers, &format!("multipart/form-data; boundary={boundary}"));
            return Ok(Some(out));
        }
        Ok(None)
    }

    fn credential(&self, profile: &str) -> Result<(Credential, Vec<String>), ToolError> {
        let p = self.cfg.auth.get(profile).ok_or_else(|| ToolError::new(ErrorCode::AuthRequired, format!("no auth profile '{profile}'")))?;
        let secret = self
            .secrets
            .get(&p.secret)
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the credential store failed: {e}")))?
            .ok_or_else(|| ToolError::new(ErrorCode::AuthRequired, format!("the secret for '{profile}' is not set. Store it yourself with: aqua-cli secret-set {}", p.secret)))?;
        let (header, value, extra) = match p.kind {
            AuthKind::Bearer => ("Authorization".to_string(), format!("Bearer {secret}"), vec![]),
            AuthKind::Basic => {
                let blob = base64::engine::general_purpose::STANDARD.encode(format!("{}:{secret}", p.name.as_deref().unwrap_or("")));
                ("Authorization".to_string(), format!("Basic {blob}"), vec![blob])
            }
            AuthKind::Header => (p.name.clone().unwrap_or_else(|| "X-API-Key".into()), secret.clone(), vec![]),
        };
        let mut masked = vec![secret];
        masked.extend(extra);
        Ok((Credential { header, value, allowed: p.clone() }, masked))
    }

    fn request(&self, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        self.validate_request(a)?;
        let method = self.method(a)?;
        let url = self.build_url(a)?;
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(h) = a.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in h {
                headers.push((k.clone(), v.as_str().unwrap_or("").to_string()));
            }
        }
        let body = self.body(a, &mut headers)?;
        let (credential, secrets) = match a.get("auth").and_then(|v| v.as_str()).filter(|n| !n.is_empty()) {
            Some(name) => {
                let (c, s) = self.credential(name)?;
                (Some(c), s)
            }
            None => (None, Vec::new()),
        };
        let save_to = match a.get("save_to").and_then(|v| v.as_str()).filter(|p| !p.is_empty()) {
            Some(p) => {
                let r = self.guard.resolve(p, Op::Write)?;
                if r.path.parent().map(|d| !d.is_dir()).unwrap_or(true) {
                    return Err(ToolError::new(ErrorCode::NotFound, format!("the folder for {} does not exist; create it first with fs.write kind=dir", display(&r.path))));
                }
                if r.path.exists() && !a.get("overwrite").and_then(|v| v.as_bool()).unwrap_or(false) {
                    return Err(ToolError::new(ErrorCode::AlreadyExists, format!("{} already exists; set overwrite=true to replace it", display(&r.path))));
                }
                Some(r.path)
            }
            None => None,
        };
        let max_bytes = if save_to.is_some() { MAX_DOWNLOAD_BYTES } else { a.get("max_response_kb").and_then(|v| v.as_u64()).unwrap_or(256).clamp(1, 2048) as usize * 1024 };
        let retries = a.get("retries").and_then(|v| v.as_u64()).unwrap_or(0).min(3);
        let req = Request {
            method: method.clone(),
            url,
            headers,
            body,
            credential,
            timeout: Duration::from_secs(a.get("timeout_s").and_then(|v| v.as_u64()).unwrap_or(30).clamp(1, 120)),
            max_bytes,
            follow_redirects: a.get("follow_redirects").and_then(|v| v.as_bool()).unwrap_or(true),
            max_redirects: 5,
        };
        let resp = self.send_with_retries(req, retries as usize, cancel)?;
        let mut out = describe(&resp, save_to.as_deref())?;
        out.secrets = secrets;
        Ok(out)
    }

    fn send_with_retries(&self, req: Request, retries: usize, cancel: &AtomicBool) -> Result<Response, ToolError> {
        let idempotent = is_idempotent(&req.method.to_ascii_uppercase());
        let mut attempt = 0usize;
        loop {
            let result = self.client.execute(&req, cancel);
            let retryable = match &result {
                Ok(r) => matches!(r.status, 429 | 502 | 503 | 504),
                Err(e) => matches!(e.code, ErrorCode::Unavailable | ErrorCode::Timeout),
            };
            if !(retryable && idempotent && attempt < retries) {
                return result;
            }
            let wait_ms = match &result {
                Ok(r) => r.headers.iter().find(|(k, _)| k == "retry-after").and_then(|(_, v)| v.trim().parse::<u64>().ok()).map(|s| s.min(10) * 1000),
                Err(_) => None,
            }
            .unwrap_or(500u64 << attempt.min(4));
            let until = std::time::Instant::now() + Duration::from_millis(wait_ms.min(10_000));
            while std::time::Instant::now() < until {
                if cancel.load(Ordering::SeqCst) {
                    return Err(ToolError::new(ErrorCode::Cancelled, "the request was cancelled"));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            attempt += 1;
        }
    }

    fn fetch(&self, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let max_chars = a.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(6000).clamp(500, 20_000) as usize;
        let req = Request { method: "GET".into(), url: arg_str(a, "url").to_string(), headers: vec![("Accept".into(), "text/html,text/plain,application/json;q=0.9,*/*;q=0.5".into())], body: None, credential: None, timeout: Duration::from_secs(30), max_bytes: 2 << 20, follow_redirects: true, max_redirects: 5 };
        let resp = self.send_with_retries(req, 1, cancel)?;
        if let Some(blocked) = &resp.blocked_redirect {
            return Ok(ProviderOutput { data: json!({"status": 0, "redirect_blocked": blocked, "url": resp.url}), text: format!("The page redirects to {blocked}, which is not on the allow list. Fetch that address directly if you want it (you will be asked)."), ..Default::default() });
        }
        let ct = header(&resp, "content-type").to_ascii_lowercase();
        let raw = String::from_utf8_lossy(&resp.body).to_string();
        let (title, text) = if ct.contains("html") || (ct.is_empty() && raw.trim_start().starts_with('<')) {
            let p = html::to_text(&raw);
            (p.title, p.text)
        } else if ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") {
            (String::new(), raw)
        } else {
            (String::new(), format!("({} bytes of {})", resp.body.len(), if ct.is_empty() { "unknown content" } else { &ct }))
        };
        let cut = text.chars().count() > max_chars;
        let shown: String = text.chars().take(max_chars).collect();
        let mut out = String::new();
        if resp.status >= 400 {
            out.push_str(&format!("HTTP {} {}\n", resp.status, resp.status_text));
        }
        if !title.is_empty() {
            out.push_str(&format!("Title: {title}\n"));
        }
        out.push_str(&format!("URL: {}\n\n{shown}", resp.url));
        if cut || resp.truncated {
            out.push_str("\n… [page cut]");
        }
        Ok(ProviderOutput { data: json!({"status": resp.status, "url": resp.url, "title": title, "chars": text.chars().count(), "truncated": cut || resp.truncated, "redirects": resp.redirects}), text: out, ..Default::default() })
    }

    /// The search services to ask, in order: the one the person set up, the fallbacks they listed, and Wikipedia last (it needs no key).
    fn search_plan(&self, query: &str, limit: usize) -> Result<Vec<SearchPlan>, ToolError> {
        if query.contains('\0') {
            return Err(bad("the query contains a NUL character"));
        }
        let mut plans: Vec<SearchPlan> = Vec::new();
        for s in self.cfg.search.iter().chain(self.cfg.search_fallback.iter()) {
            plans.push(match s.engine {
                SearchEngine::Searxng => {
                    let mut u = url::Url::parse(s.url.as_deref().unwrap_or("")).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the searxng url is not valid: {e}")))?;
                    u.set_path("/search");
                    u.query_pairs_mut().append_pair("q", query).append_pair("format", "json").append_pair("language", "en");
                    SearchPlan { engine: s.engine, url: u.to_string(), profile: None, body: None }
                }
                SearchEngine::Brave => {
                    // `url` is the service's own address unless the person points it elsewhere (a proxy, or a test).
                    let base = s.url.as_deref().unwrap_or("https://api.search.brave.com").trim_end_matches('/');
                    let mut u = url::Url::parse(&format!("{base}/res/v1/web/search")).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the brave url is not valid: {e}")))?;
                    u.query_pairs_mut().append_pair("q", query).append_pair("count", &limit.to_string());
                    SearchPlan { engine: s.engine, url: u.to_string(), profile: s.auth.clone(), body: None }
                }
                SearchEngine::Tavily => {
                    let base = s.url.as_deref().unwrap_or("https://api.tavily.com").trim_end_matches('/');
                    SearchPlan { engine: s.engine, url: format!("{base}/search"), profile: s.auth.clone(), body: Some(json!({"query": query, "max_results": limit, "search_depth": "basic", "include_answer": false})) }
                }
                SearchEngine::Wikipedia => self.wikipedia_plan(s.url.as_deref(), query, limit)?,
                SearchEngine::DuckDuckGo => self.duckduckgo_plan(s.url.as_deref(), query)?,
                SearchEngine::Parallel => {
                    // An MCP server spoken to over HTTP: one `tools/call`, no handshake (the keyless service answers it without one). The objective is the query; the keyword
                    // queries are its first words.
                    let base = s.url.as_deref().unwrap_or("https://search.parallel.ai").trim_end_matches('/');
                    let keywords: String = query.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
                    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "web_search", "arguments": {"objective": query, "search_queries": [keywords]}}});
                    SearchPlan { engine: s.engine, url: format!("{base}/mcp"), profile: s.auth.clone(), body: Some(call) }
                }
                SearchEngine::Ollama => {
                    let base = s.url.as_deref().unwrap_or("https://ollama.com").trim_end_matches('/');
                    SearchPlan { engine: s.engine, url: format!("{base}/api/web_search"), profile: s.auth.clone(), body: Some(json!({"query": query, "max_results": limit})) }
                }
            });
        }
        // No key needed: DuckDuckGo's results page, asked before Wikipedia so that a question about something that has no article is not answered with nothing.
        if duckduckgo_enabled() && !plans.iter().any(|p| p.engine == SearchEngine::DuckDuckGo) {
            plans.push(self.duckduckgo_plan(None, query)?);
        }
        if !plans.iter().any(|p| p.engine == SearchEngine::Wikipedia) {
            plans.push(self.wikipedia_plan(None, query, limit)?);
        }
        Ok(plans)
    }

    /// May this service be asked? The network policy lets the address through and, when the service carries a credential, that credential may
    /// go to that host (the auth profile names it) and only over https or to this computer: the two rules `http.request` applies, so a search
    /// cannot send a key where the same key could not be sent by hand. `Ok` is the host.
    fn search_service_ok(&self, plan: &SearchPlan) -> Result<String, String> {
        let t = NetPolicy::parse(&plan.url)?;
        if let Verdict::Deny(why) = self.client.policy().check(&t) {
            return Err(why);
        }
        if let Some(name) = &plan.profile {
            let profile = self.cfg.auth.get(name).ok_or_else(|| format!("no auth profile '{name}'"))?;
            if !self.client.policy().profile_allows(profile, &t.host, t.port) {
                return Err(format!("the '{name}' credential may not be sent to {}", t.host));
            }
            if !NetPolicy::credential_transport_ok(&t) {
                return Err(format!("the '{name}' credential is only sent over https (this address is plain http)"));
            }
        }
        Ok(t.host.clone())
    }

    fn duckduckgo_plan(&self, base: Option<&str>, query: &str) -> Result<SearchPlan, ToolError> {
        let base = base.unwrap_or("https://html.duckduckgo.com").trim_end_matches('/');
        let mut u = url::Url::parse(&format!("{base}/html/")).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the duckduckgo url is not valid: {e}")))?;
        u.query_pairs_mut().append_pair("q", query);
        Ok(SearchPlan { engine: SearchEngine::DuckDuckGo, url: u.to_string(), profile: None, body: None })
    }

    fn wikipedia_plan(&self, base: Option<&str>, query: &str, limit: usize) -> Result<SearchPlan, ToolError> {
        let base = base.unwrap_or("https://en.wikipedia.org").trim_end_matches('/');
        let mut u = url::Url::parse(&format!("{base}/w/api.php")).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the wikipedia url is not valid: {e}")))?;
        u.query_pairs_mut().append_pair("action", "query").append_pair("list", "search").append_pair("srsearch", query).append_pair("srlimit", &limit.to_string()).append_pair("srprop", "snippet|timestamp").append_pair("format", "json").append_pair("utf8", "1");
        Ok(SearchPlan { engine: SearchEngine::Wikipedia, url: u.to_string(), profile: None, body: None })
    }

    fn search(&self, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let query = arg_str(a, "query");
        let limit = a.get("limit").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 10) as usize;
        let mut last_error: Option<ToolError> = None;
        let mut tried: Vec<String> = Vec::new();
        // What has the shape of a secret, a card, an identity number or a file of this computer is not sent to any service, and the model is told what it was (never the text).
        if let Some(found) = sensitive::find(query, false) {
            return Err(ToolError::new(ErrorCode::InvalidArguments, found.refusal()));
        }
        for plan in self.search_plan(query, limit)? {
            let name = plan.engine.name();
            // A service that may keep or train on what it is sent is also not sent an email address, a telephone number or an address on the person's network: it is skipped,
            // and the next one is asked.
            if plan.engine.may_keep_queries() {
                if let Some(found) = sensitive::find(query, true) {
                    tried.push(format!("{name} (not sent: the query has {})", found.what));
                    continue;
                }
            }
            // A service the network policy refuses, or one whose credential may not go to that address (the rules `http.request` applies), is
            // skipped, like one that fails.
            if let Err(why) = self.search_service_ok(&plan) {
                tried.push(format!("{name} (refused: {})", why.chars().take(120).collect::<String>()));
                last_error = Some(ToolError::new(ErrorCode::NetworkBlocked, why));
                continue;
            }
            if let Some(hits) = self.searches.get(name, query, limit) {
                let (text, data) = search::render(query, &hits);
                let mut data = data;
                data["provider"] = json!(name);
                data["cached"] = json!(true);
                return Ok(ProviderOutput { data, text: format!("{text}\n(via {name}, from a search made a moment ago)"), ..Default::default() });
            }
            match self.search_with(&plan, limit, cancel) {
                Ok((hits, secrets)) if !hits.is_empty() => {
                    let hits = search::dedupe(hits);
                    self.searches.put(name, query, limit, &hits, search::ttl_for(query, plan.engine == SearchEngine::Brave));
                    let (text, mut data) = search::render(query, &hits);
                    data["provider"] = json!(name);
                    data["cached"] = json!(false);
                    let note = if tried.is_empty() { String::new() } else { format!("; tried first: {}", tried.join(", ")) };
                    return Ok(ProviderOutput { data, text: format!("{text}\n(via {name}{note})"), secrets, ..Default::default() });
                }
                Ok(_) => tried.push(format!("{name} (no results)")),
                Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                Err(e) => {
                    tried.push(format!("{name} ({})", e.message.chars().take(80).collect::<String>()));
                    last_error = Some(e);
                }
            }
        }
        match last_error {
            Some(e) if tried.iter().all(|t| !t.contains("no results")) => Err(e),
            _ => Ok(ProviderOutput { data: json!({"query": query, "results": [], "tried": tried}), text: format!("No results. Tried: {}.", if tried.is_empty() { "nothing".into() } else { tried.join(", ") }), ..Default::default() }),
        }
    }

    /// Asks one service. Returns its hits, and the secrets that were used (the host masks them).
    fn search_with(&self, plan: &SearchPlan, limit: usize, cancel: &AtomicBool) -> Result<(Vec<search::Hit>, Vec<String>), ToolError> {
        let (credential, secrets) = match &plan.profile {
            Some(p) => {
                let (c, s) = self.credential(p)?;
                (Some(c), s)
            }
            None => (None, Vec::new()),
        };
        let accept = match plan.engine {
            SearchEngine::DuckDuckGo => "text/html",
            // An MCP server over HTTP may answer with plain JSON or with one event.
            SearchEngine::Parallel => "application/json, text/event-stream",
            _ => "application/json",
        };
        let mut headers = vec![("Accept".to_string(), accept.to_string())];
        let body = plan.body.as_ref().map(|b| {
            headers.push(("Content-Type".into(), "application/json".into()));
            serde_json::to_vec(b).unwrap_or_default()
        });
        let method = if body.is_some() { "POST" } else { "GET" };
        let req = Request { method: method.into(), url: plan.url.clone(), headers, body, credential, timeout: Duration::from_secs(20), max_bytes: 1 << 20, follow_redirects: false, max_redirects: 0 };
        let resp = self.send_with_retries(req, 1, cancel)?;
        // DuckDuckGo answers a check instead of results (HTTP 202) when it sees many automated requests: that is a rate limit, and the next service is asked.
        if plan.engine == SearchEngine::DuckDuckGo && resp.status == 202 {
            return Err(ToolError::new(ErrorCode::RateLimited, "duckduckgo asked for a check instead of showing results (it does that when it sees many automated requests)".to_string()));
        }
        if resp.status != 200 {
            return Err(ToolError::new(if resp.status == 401 || resp.status == 403 { ErrorCode::AuthFailed } else if resp.status == 429 { ErrorCode::RateLimited } else { ErrorCode::BackendError }, format!("{} answered HTTP {}", plan.engine.name(), resp.status)));
        }
        let mut hits = if plan.engine == SearchEngine::DuckDuckGo {
            search::parse_duckduckgo(&String::from_utf8_lossy(&resp.body))
        } else if plan.engine == SearchEngine::Parallel {
            search::parse_parallel_body(&resp.body)
        } else {
            let v: Value = serde_json::from_slice(&resp.body).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("{} sent something that is not JSON: {e}", plan.engine.name())))?;
            match plan.engine {
                SearchEngine::Brave => search::parse_brave(&v),
                SearchEngine::Searxng => search::parse_searxng(&v),
                SearchEngine::Tavily => search::parse_tavily(&v),
                SearchEngine::Wikipedia => search::parse_wikipedia(&v),
                SearchEngine::Ollama => search::parse_ollama(&v),
                SearchEngine::DuckDuckGo | SearchEngine::Parallel => Vec::new(),
            }
        };
        hits.truncate(limit);
        Ok((hits, secrets))
    }

    /// `web.read`: the part of a page that answers a goal.
    fn read(&self, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let max_chars = a.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(2400).clamp(500, 8000) as usize;
        let req = Request { method: "GET".into(), url: arg_str(a, "url").to_string(), headers: vec![("Accept".into(), "text/html,text/plain,application/json;q=0.9,*/*;q=0.5".into())], body: None, credential: None, timeout: Duration::from_secs(30), max_bytes: 2 << 20, follow_redirects: true, max_redirects: 5 };
        let resp = self.send_with_retries(req, 1, cancel)?;
        if let Some(blocked) = &resp.blocked_redirect {
            return Ok(ProviderOutput { data: json!({"status": 0, "redirect_blocked": blocked}), text: format!("The page redirects to {blocked}, which is not on the allow list, so it was not read."), ..Default::default() });
        }
        let ct = header(&resp, "content-type").to_ascii_lowercase();
        let body = String::from_utf8_lossy(&resp.body).to_string();
        let mut out = String::new();
        if resp.status >= 400 {
            out.push_str(&format!("HTTP {} {}\n", resp.status, resp.status_text));
        }
        let (page, data_extra) = if ct.contains("html") || ct.is_empty() && body.trim_start().starts_with('<') {
            let p = read::extract(&body, &resp.url, arg_str(a, "goal"), max_chars);
            let extra = json!({"title": p.title, "date": p.date, "blocks_found": p.blocks_found, "blocks_kept": p.blocks_kept, "links": p.links.iter().map(|(t, u)| json!({"text": t, "url": u})).collect::<Vec<_>>()});
            out.push_str(&read::render(&p, &resp.url));
            (p, extra)
        } else if ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") {
            let cut: String = body.chars().take(max_chars).collect();
            out.push_str(&format!("URL: {}\n\n{cut}", resp.url));
            if body.chars().count() > max_chars {
                out.push_str("\n… [cut]");
            }
            (read::Extracted::default(), json!({}))
        } else {
            out.push_str(&format!("URL: {}\n\n({} bytes of {}: not text)", resp.url, resp.body.len(), if ct.is_empty() { "unknown content" } else { &ct }));
            (read::Extracted::default(), json!({}))
        };
        let _ = page;
        let mut data = json!({"status": resp.status, "url": resp.url, "redirects": resp.redirects, "truncated": resp.truncated});
        if let (Some(d), Some(e)) = (data.as_object_mut(), data_extra.as_object()) {
            d.extend(e.clone());
        }
        Ok(ProviderOutput { data, text: out, ..Default::default() })
    }

    fn scrape(&self, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        if !self.cfg.scrape {
            return Err(ToolError::new(ErrorCode::Unavailable, "web scraping is switched off"));
        }
        let url_str = arg_str(a, "url");
        let css_selector = arg_str(a, "css_selector");
        let stealth = a.get("stealth").and_then(|v| v.as_bool()).unwrap_or(true);

        // The browser engine follows redirects and loads sub-requests on its own, with the system resolver, so the
        // network policy cannot see them. The address is therefore walked first with Aqua's own client, which checks
        // every hop (a redirect to a private or metadata address is refused there), and the browser is only given the
        // address that walk ended at.
        let probe = Request { method: "GET".into(), url: url_str.to_string(), headers: vec![], body: None, credential: None, timeout: Duration::from_secs(30), max_bytes: 64 << 10, follow_redirects: true, max_redirects: 5 };
        let walked = self.client.execute(&probe, cancel)?;
        if let Some(blocked) = &walked.blocked_redirect {
            return Err(ToolError::new(ErrorCode::NetworkBlocked, format!("the page redirects to {blocked}, which is not allowed, so it was not opened")));
        }
        let url = walked.url.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let job_url = url.clone();
        std::thread::spawn(move || {
            let r = if stealth { scrapling::fetchers::StealthyFetcher::fetch(&job_url) } else { scrapling::fetchers::DynamicFetcher::fetch(&job_url) };
            let _ = tx.send(r);
        });
        // A deadline, and the run's cancel flag: the thread cannot be killed, but the call no longer waits for it.
        let deadline = std::time::Instant::now() + Duration::from_secs(SCRAPE_MAX_SECS);
        let document_result = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(r) => break r,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(ToolError::new(ErrorCode::Cancelled, "the request was cancelled"));
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(ToolError::new(ErrorCode::Timeout, format!("the page did not finish loading within {SCRAPE_MAX_SECS} seconds")));
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(ToolError::new(ErrorCode::BackendError, "the page loader stopped unexpectedly")),
            }
        };

        let document = document_result.map_err(|e| ToolError::new(ErrorCode::BackendError, format!("scrapling fetch failed: {e}")))?;
        // The page may still have moved itself (a script, a meta refresh). Where it ended up must pass the same policy
        // before anything it showed is handed to the model.
        if let Ok(end) = NetPolicy::parse(&document.url) {
            if let Verdict::Deny(why) = self.client.policy().check(&end) {
                return Err(ToolError::new(ErrorCode::NetworkBlocked, format!("the page ended at {}: {why}", end.host)));
            }
        }
        
        let mut extracted = String::new();
        if !css_selector.is_empty() {
            if let Ok(elements) = document.css(css_selector) {
                extracted = elements.getall().to_strings().join("\n");
            }
        }
        
        let text_output = if !extracted.is_empty() {
            extracted
        } else {
            crate::http::html::to_text(&String::from_utf8_lossy(document.body())).text
        };
        
        let truncated = text_output.chars().take(20000).collect::<String>();

        Ok(ProviderOutput {
            data: json!({"url": url_str, "status": document.status, "chars": truncated.len()}),
            text: truncated,
            ..Default::default()
        })
    }
}

/// How long a scraped page may take to load before the call gives up on it.
const SCRAPE_MAX_SECS: u64 = 90;

fn header<'a>(r: &'a Response, name: &str) -> &'a str {
    r.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
}

/// Turn a response into tool output. HTTP errors (404, 500…) are results, not tool failures.
fn describe(r: &Response, save_to: Option<&std::path::Path>) -> Result<ProviderOutput, ToolError> {
    if let Some(blocked) = &r.blocked_redirect {
        return Ok(ProviderOutput { data: json!({"status": 0, "redirect_blocked": blocked, "redirects": r.redirects}), text: format!("The server redirected to {blocked}, which is not on the allow list, so it was not followed. Make a new request to that address if you want it."), ..Default::default() });
    }
    let ct = header(r, "content-type").to_ascii_lowercase();
    let headers: serde_json::Map<String, Value> = r.headers.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
    let mut data = json!({"status": r.status, "status_text": r.status_text, "url": r.url, "headers": headers, "redirects": r.redirects, "truncated": r.truncated, "elapsed_ms": r.elapsed_ms, "bytes": r.body.len()});
    let mut artifacts = Vec::new();
    let mut text = format!("HTTP {} {}", r.status, r.status_text);
    if let Some(path) = save_to {
        if r.truncated {
            return Err(ToolError::new(ErrorCode::ResourceLimit, format!("the download is larger than {} bytes and was not saved", MAX_DOWNLOAD_BYTES)));
        }
        if !(200..300).contains(&r.status) {
            return Ok(ProviderOutput { data, text: format!("{text}\nNothing was saved because the server did not answer 2xx."), ..Default::default() });
        }
        std::fs::write(path, &r.body).map_err(|e| ToolError::new(ErrorCode::OsPermissionDenied, format!("cannot write {}: {e}", display(path))))?;
        artifacts.push(Artifact { kind: "file".into(), path: path.to_string_lossy().to_string(), bytes: r.body.len() as u64, sha256: Sha256::digest(&r.body).iter().map(|b| format!("{b:02x}")).collect() });
        data["saved_to"] = json!(display(path));
        text.push_str(&format!("\nSaved {} bytes to {}", r.body.len(), display(path)));
        return Ok(ProviderOutput { data, text, artifacts, ..Default::default() });
    }
    let is_text = ct.is_empty() || ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") || ct.contains("javascript") || ct.contains("x-www-form-urlencoded");
    if is_text {
        let body = String::from_utf8_lossy(&r.body).to_string();
        if ct.contains("json") && !r.truncated {
            if let Ok(j) = serde_json::from_slice::<Value>(&r.body) {
                data["json"] = j;
            }
        }
        data["body"] = json!(body);
        if !body.is_empty() {
            text.push_str(&format!("\n\n{body}"));
        }
        if r.truncated {
            text.push_str("\n… [response cut at the size limit]");
        }
    } else if !r.body.is_empty() {
        text.push_str(&format!("\n({} bytes of {}; use save_to to keep it)", r.body.len(), if ct.is_empty() { "binary data" } else { &ct }));
    }
    Ok(ProviderOutput { data, text, artifacts, ..Default::default() })
}
