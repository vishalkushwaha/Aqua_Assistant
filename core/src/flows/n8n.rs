//! Phase 9: calling n8n.
//!
//! n8n is a separate program (a workflow tool with its own connectors) that the person runs on this computer. Aqua
//! never bundles, starts or manages it, and nothing else depends on it being there. This is one optional step that
//! calls a workflow through its Webhook node, and a health check that says whether n8n is reachable.
//!
//! - **Set up explicitly.** Nothing is assumed about a port: `AQUA_N8N_URL` (for example `http://127.0.0.1:5678`)
//!   must be set, and it must be on this computer, so what a flow sends is never sent anywhere else by this step.
//! - **It asks first.** An n8n workflow can do anything n8n can do, so the step is high-impact: it asks before it
//!   runs (the same rules as publishing), and it is in the no-double-action ledger.
//! - **Secrets by name.** The webhook's password is a credential named on the step, read from the credential store at
//!   the moment of the call and sent in a header (`X-Aqua-Token` unless the step says otherwise). It is never in the
//!   flow, the log or an error message.
//! - **Honest about what it knows.** If nothing could be sent (n8n is not running), the call is retryable. If the
//!   request went out and the answer was lost or was a server error, the workflow may have run, so the step stops as
//!   "unknown" for a person to check; it is never repeated by itself. n8n is sent an `Idempotency-Key` so a workflow
//!   that wants to can ignore a repeat.

use crate::flows::model::NodeKind;
use crate::flows::run::exec::{NodeCtx, NodeError, NodeExecutor, NodeResult};
use aqua_native_tools::secrets::Secrets;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

/// The most of an answer that is read.
pub const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// The n8n address from the environment, checked to be on this computer.
pub fn configured_base() -> Result<Option<String>, String> {
    match std::env::var("AQUA_N8N_URL").ok().map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(u) => check_base(&u).map(Some),
    }
}

pub fn check_base(url: &str) -> Result<String, String> {
    let host = super::net::http_host(url).ok_or("the n8n address must start with http:// and be on this computer")?;
    if !super::net::is_local_http(url) {
        return Err(format!("the n8n address must be on this computer (127.0.0.1 or localhost), not {host}: what a flow sends is not sent elsewhere by this step"));
    }
    Ok(url.trim_end_matches('/').to_string())
}

const NOT_SET_UP: &str = "n8n is not set up: run n8n on this computer and set AQUA_N8N_URL (for example http://127.0.0.1:5678). Nothing was sent";

/// A webhook path: letters, digits, `_ - /`, no way to climb out of `/webhook/`.
fn valid_path(p: &str) -> bool {
    !p.is_empty() && p.len() <= 200 && !p.starts_with('/') && !p.contains("//") && !p.split('/').any(|s| s == ".." || s == ".") && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/'))
}

fn valid_header(h: &str) -> bool {
    !h.is_empty() && h.len() <= 60 && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !matches!(h.to_ascii_lowercase().as_str(), "host" | "content-length" | "content-type" | "transfer-encoding" | "connection" | "idempotency-key")
}

// ---- health -----------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Health {
    pub configured: bool,
    pub reachable: bool,
    pub base_url: Option<String>,
    /// In words: what was found, or what to do.
    pub detail: String,
}

/// Is n8n there? Asks `GET /healthz`, which n8n answers with 200 when it is up.
pub fn health(base: Result<Option<String>, String>, timeout: Duration) -> Health {
    let base = match base {
        Ok(Some(b)) => b,
        Ok(None) => return Health { configured: false, reachable: false, base_url: None, detail: NOT_SET_UP.into() },
        Err(e) => return Health { configured: false, reachable: false, base_url: None, detail: e },
    };
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    match agent.get(&format!("{base}/healthz")).call() {
        Ok(r) if r.status() == 200 => Health { configured: true, reachable: true, base_url: Some(base), detail: "n8n answered: it is up".into() },
        Ok(r) => Health { configured: true, reachable: false, base_url: Some(base), detail: format!("n8n answered with {}, so it may not be ready", r.status()) },
        Err(ureq::Error::Status(code, _)) => Health { configured: true, reachable: false, base_url: Some(base), detail: format!("n8n answered with {code} at /healthz; is this address really n8n?") },
        Err(e) => Health { configured: true, reachable: false, base_url: Some(base.clone()), detail: format!("could not connect to n8n at {base}: is it running? ({})", e.to_string().chars().take(100).collect::<String>()) },
    }
}

// ---- the step ----------------------------------------------------------------------------------------------------------------

pub struct N8nNodeExecutor {
    base: Result<Option<String>, String>,
    secrets: Arc<dyn Secrets>,
}

impl N8nNodeExecutor {
    /// Reads `AQUA_N8N_URL` once, when the executor is made.
    pub fn from_env(secrets: Arc<dyn Secrets>) -> Arc<Self> {
        Arc::new(Self { base: configured_base(), secrets })
    }

    pub fn new(base: Option<&str>, secrets: Arc<dyn Secrets>) -> Arc<Self> {
        Arc::new(Self { base: base.map(check_base).transpose(), secrets })
    }
}

fn text(cfg: &Value, key: &str) -> String {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
}

fn payload_of(cfg: &Value) -> Result<Value, NodeError> {
    match cfg.get("payload") {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Value::Null),
        Some(Value::String(s)) => serde_json::from_str(s).map_err(|_| NodeError::Fatal("invalid setting: the data to send must be valid JSON".into())),
        Some(v) => Ok(v.clone()),
    }
}

/// A flat object as a query string, for GET.
fn query_of(v: &Value) -> Result<Vec<(String, String)>, NodeError> {
    match v {
        Value::Null => Ok(vec![]),
        Value::Object(m) => Ok(m.iter().map(|(k, x)| (k.clone(), match x { Value::String(s) => s.clone(), other => other.to_string() })).collect()),
        _ => Err(NodeError::Fatal("invalid setting: with GET the data to send must be a JSON object".into())),
    }
}

impl NodeExecutor for N8nNodeExecutor {
    fn kinds(&self) -> &[NodeKind] {
        &[NodeKind::N8n]
    }

    fn execute(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
        let cfg = &ctx.config;
        let base = match &self.base {
            Ok(Some(b)) => b.clone(),
            Ok(None) => return Err(NodeError::Fatal(NOT_SET_UP.into())),
            Err(e) => return Err(NodeError::Fatal(format!("invalid setting: {e}"))),
        };
        let path = text(cfg, "workflow");
        if !valid_path(&path) {
            return Err(NodeError::Fatal(format!("invalid setting: \"{path}\" is not a webhook path (letters, digits, - _ and /, for example post-to-sheet)")));
        }
        let method = match text(cfg, "method").as_str() {
            "" | "POST" => "POST",
            "GET" => "GET",
            other => return Err(NodeError::Fatal(format!("invalid setting: the method \"{other}\" is not supported (POST or GET)"))),
        };
        let prefix = match text(cfg, "mode").as_str() {
            "" | "production" => "webhook",
            "test" => "webhook-test",
            other => return Err(NodeError::Fatal(format!("invalid setting: \"{other}\" is not an address to use (production or test)"))),
        };
        let timeout = cfg.get("timeout_secs").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, MAX_TIMEOUT_SECS);
        let payload = payload_of(cfg)?;
        let header_name = match text(cfg, "auth_header").as_str() {
            "" => "X-Aqua-Token".to_string(),
            h if valid_header(h) => h.to_string(),
            h => return Err(NodeError::Fatal(format!("invalid setting: \"{h}\" cannot be used as the credential's header"))),
        };
        // The credential is read now, used once, and never written anywhere.
        let secret = match &ctx.node.credential {
            Some(name) => match self.secrets.get(name) {
                Ok(Some(v)) => Some(v),
                Ok(None) => return Err(NodeError::Fatal(format!("not logged in: the credential \"{name}\" is not saved, so the webhook cannot be called with it. Save it in the credential store, then run this step again"))),
                Err(e) => return Err(NodeError::Fatal(format!("invalid setting: {e}"))),
            },
            None => None,
        };

        let url = format!("{base}/{prefix}/{path}");
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(timeout)).redirects(0).build();
        let mut req = if method == "GET" { agent.get(&url) } else { agent.post(&url) };
        req = req.set("Idempotency-Key", &format!("aqua-run{}-{}", ctx.run_id, ctx.node.id)).set("X-Aqua-Run", &ctx.run_id.to_string()).set("X-Aqua-Step", &ctx.node.id).set("Accept", "application/json");
        if let Some(s) = &secret {
            req = req.set(&header_name, s);
        }
        let sent = if method == "GET" {
            for (k, v) in query_of(&payload)? {
                req = req.query(&k, &v);
            }
            req.call()
        } else if payload.is_null() {
            req.send_json(json!({}))
        } else {
            req.send_json(payload)
        };
        let resp = match sent {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = read_capped(r);
                let hint: String = body.chars().take(160).collect();
                return Err(match code {
                    401 | 403 => NodeError::Fatal(format!("{code} unauthorized: n8n refused the credential (check the webhook's Header Auth and the credential saved for this step)")),
                    404 => NodeError::Fatal(format!("the webhook \"{path}\" was not found in n8n (404). {}", if prefix == "webhook" { "The production address only works when the workflow is active; switch it on in n8n, or use the test address while building." } else { "The test address only works while n8n is waiting for a test event." })),
                    429 => NodeError::Retryable("429 too many requests: n8n asked to slow down. Nothing was run".into()),
                    400..=499 => NodeError::Fatal(format!("n8n refused the request ({code}): {hint}")),
                    // The request arrived, so the workflow may have run, wholly or in part.
                    _ => NodeError::Unknown(format!("n8n answered with an error ({code}) after the request was sent, so it is not known whether the workflow ran (wholly or in part). Check n8n's executions before running this step again. {}", if hint.is_empty() { String::new() } else { format!("It said: {hint}") })),
                });
            }
            Err(ureq::Error::Transport(t)) => {
                let m = t.to_string().chars().take(120).collect::<String>();
                return Err(match t.kind() {
                    // Nothing reached n8n.
                    ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns => NodeError::Retryable(format!("could not connect to n8n at {base} (connection error: is it running?) {m}")),
                    // It may have been sent and even run.
                    _ => NodeError::Unknown(format!("the connection to n8n was lost or timed out after the request was sent ({m}), so it is not known whether the workflow ran. Check n8n's executions before running this step again")),
                });
            }
        };
        let status = resp.status();
        if (300..400).contains(&status) {
            return Err(NodeError::Fatal(format!("n8n answered with a redirect ({status}); a webhook address does not redirect. Check the path")));
        }
        let is_json = resp.content_type().contains("json");
        let body = read_capped(resp);
        let result: Value = if is_json { serde_json::from_str(&body).unwrap_or(Value::String(body.clone())) } else if body.is_empty() { Value::Null } else { Value::String(body.clone()) };
        let mut o = Map::new();
        o.insert("result".into(), result);
        o.insert("status".into(), json!(status));
        Ok(NodeResult { outputs: o, logs: vec![format!("Called the n8n workflow \"{path}\" ({method}): {status}, {} bytes back", body.len())] })
    }
}

fn read_capped(r: ureq::Response) -> String {
    let mut buf = Vec::new();
    let _ = r.into_reader().take(MAX_RESPONSE_BYTES).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}
