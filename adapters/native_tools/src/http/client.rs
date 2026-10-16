//! The HTTP engine: one request, with redirects followed by hand so that every hop passes the
//! network policy, and with connections made only to addresses Aqua's own resolver vetted.

use super::policy::{NetPolicy, Verdict};
use aqua_tool_host::{ErrorCode, ToolError};
use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const USER_AGENT: &str = "Aqua/0.1 (local assistant)";

/// Response headers that are passed on. `Set-Cookie` and anything that may carry a session or token is dropped.
const SAFE_RESPONSE_HEADERS: &[&str] = &["content-type", "content-length", "content-encoding", "location", "etag", "last-modified", "retry-after", "cache-control", "date", "content-language", "content-disposition", "x-ratelimit-remaining", "x-ratelimit-reset"];

/// A credential ready to be sent, and the hosts it may go to.
#[derive(Clone)]
pub struct Credential {
    pub header: String,
    pub value: String,
    pub allowed: super::policy::AuthProfile,
}

pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub credential: Option<Credential>,
    pub timeout: Duration,
    pub max_bytes: usize,
    pub follow_redirects: bool,
    pub max_redirects: usize,
}

pub struct Response {
    pub status: u16,
    pub status_text: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub truncated: bool,
    pub redirects: Vec<String>,
    /// A redirect that was not followed because its target needs the user's approval or is refused.
    pub blocked_redirect: Option<String>,
    pub elapsed_ms: u64,
}

struct PolicyResolver(Arc<NetPolicy>);

impl ureq::Resolver for PolicyResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        self.0.resolve(netloc)
    }
}

pub struct Client {
    policy: Arc<NetPolicy>,
}

fn blocked(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::NetworkBlocked, msg)
}

impl Client {
    pub fn new(policy: Arc<NetPolicy>) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &Arc<NetPolicy> {
        &self.policy
    }

    pub fn execute(&self, req: &Request, cancel: &AtomicBool) -> Result<Response, ToolError> {
        self.execute_keeping(req, cancel, &[])
    }

    /// [`execute`](Client::execute) that also passes on the response headers named in `keep` (any case), besides the ones that are always passed on. For a caller that is not the model's `http.request`
    /// and needs one more header of the answer (the session of a remote MCP server, card OA11-03); nothing it does not name is passed on, and `Set-Cookie` never is unless it is named.
    pub fn execute_keeping(&self, req: &Request, cancel: &AtomicBool, keep: &[&str]) -> Result<Response, ToolError> {
        let started = Instant::now();
        let deadline = started + req.timeout;
        let mut method = req.method.to_ascii_uppercase();
        let mut url = req.url.clone();
        let mut body = req.body.clone();
        let mut redirects: Vec<String> = Vec::new();
        loop {
            let target = NetPolicy::parse(&url).map_err(blocked)?;
            match self.policy.check(&target) {
                Verdict::Deny(why) => return Err(blocked(why)),
                // The first hop was approved with the call; later hops to an unlisted host are not.
                Verdict::Ask if !redirects.is_empty() => {
                    return Ok(Response { status: 0, status_text: String::new(), url: url.clone(), headers: vec![], body: vec![], truncated: false, redirects, blocked_redirect: Some(url), elapsed_ms: started.elapsed().as_millis() as u64 });
                }
                Verdict::Ask | Verdict::Allow => {}
            }
            if cancel.load(Ordering::SeqCst) {
                return Err(ToolError::new(ErrorCode::Cancelled, "the request was cancelled"));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ToolError::new(ErrorCode::Timeout, format!("no complete response within {} s", req.timeout.as_secs())));
            }
            let agent = ureq::AgentBuilder::new()
                .redirects(0)
                .resolver(PolicyResolver(self.policy.clone()))
                .timeout_connect(remaining.min(Duration::from_secs(10)))
                .timeout_read(Duration::from_secs(5).min(remaining))
                .timeout_write(remaining.min(Duration::from_secs(30)))
                .user_agent(USER_AGENT)
                .build();
            let mut r = agent.request(&method, &url);
            for (k, v) in &req.headers {
                r = r.set(k, v);
            }
            // A credential goes only to the hosts its profile names, and is dropped on a redirect elsewhere.
            if let Some(c) = &req.credential {
                // ...and only over https (a redirect that drops to plain http loses it for that hop).
                if self.policy.profile_allows(&c.allowed, &target.host, target.port) && NetPolicy::credential_transport_ok(&target) {
                    r = r.set(&c.header, &c.value);
                }
            }
            let sent = match &body {
                Some(b) => r.send_bytes(b),
                None => r.call(),
            };
            let resp = match sent {
                Ok(resp) => resp,
                Err(ureq::Error::Status(_, resp)) => resp,
                Err(ureq::Error::Transport(t)) => return Err(map_transport(&t, &target.host, req.timeout)),
            };
            let status = resp.status();
            let status_text = resp.status_text().to_string();
            let location = resp.header("location").map(String::from);
            let mut headers = Vec::new();
            for name in resp.headers_names() {
                if SAFE_RESPONSE_HEADERS.contains(&name.to_ascii_lowercase().as_str()) || keep.iter().any(|k| k.eq_ignore_ascii_case(&name)) {
                    if let Some(v) = resp.header(&name) {
                        headers.push((name.to_ascii_lowercase(), v.to_string()));
                    }
                }
            }
            if req.follow_redirects && matches!(status, 301 | 302 | 303 | 307 | 308) {
                if let Some(loc) = location {
                    if redirects.len() >= req.max_redirects {
                        return Err(ToolError::new(ErrorCode::ResourceLimit, format!("more than {} redirects", req.max_redirects)));
                    }
                    let next = target.url.join(&loc).map_err(|e| blocked(format!("the redirect target '{loc}' is not a valid URL: {e}")))?;
                    redirects.push(url.clone());
                    if status == 303 || ((status == 301 || status == 302) && method == "POST") {
                        method = "GET".into();
                        body = None;
                    }
                    url = next.to_string();
                    continue;
                }
            }
            let mut reader = resp.into_reader().take(req.max_bytes as u64 + 1);
            let mut data = Vec::new();
            let mut buf = [0u8; 16 * 1024];
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Err(ToolError::new(ErrorCode::Cancelled, "the request was cancelled"));
                }
                if Instant::now() >= deadline {
                    return Err(ToolError::new(ErrorCode::Timeout, format!("the response did not finish within {} s", req.timeout.as_secs())));
                }
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => data.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(ToolError::new(ErrorCode::Unavailable, format!("reading the response failed: {e}"))),
                }
            }
            let truncated = data.len() > req.max_bytes;
            data.truncate(req.max_bytes);
            return Ok(Response { status, status_text, url, headers, body: data, truncated, redirects, blocked_redirect: None, elapsed_ms: started.elapsed().as_millis() as u64 });
        }
    }
}

fn map_transport(t: &ureq::Transport, host: &str, timeout: Duration) -> ToolError {
    let msg = t.to_string();
    let low = msg.to_ascii_lowercase();
    if low.contains("private or local address") || low.contains("aqua's own services") || low.contains("permissiondenied") || low.contains("permission denied") && low.contains("resolves") {
        return blocked(msg);
    }
    // Windows reports a read timeout as "did not properly respond after a period of time" (error 10060).
    if low.contains("timed out") || low.contains("timeout") || low.contains("deadline") || low.contains("10060") || low.contains("period of time") || low.contains("did not properly respond") {
        return ToolError::new(ErrorCode::Timeout, format!("{host} did not answer within {} s", timeout.as_secs()));
    }
    if low.contains("certificate") || low.contains("tls") || low.contains("handshake") || low.contains("invalid peer") {
        return ToolError::new(ErrorCode::BackendError, format!("the secure connection to {host} failed (certificate or TLS problem): {msg}"));
    }
    // "cannot reach" means the connection was never made, so nothing was sent and trying again is safe. Callers that must not
    // repeat a write (the flow engine) rely on this wording; a failure after the request went out is worded differently.
    match t.kind() {
        ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::ProxyConnect | ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => {
            ToolError::new(ErrorCode::Unavailable, format!("cannot reach {host}: {msg}"))
        }
        _ => ToolError::new(ErrorCode::Unavailable, format!("the connection to {host} was lost after the request was sent: {msg}")),
    }
}
