//! Monitoring and log tools (`mon.*`). All read-only.
//!
//! * `mon.log_tail`  – the last lines of a log file inside the allowed folders, optionally filtered.
//! * `mon.eventlog`  – recent Windows Event Log entries (Application or System only, never Security).
//! * `mon.probe`     – checks the TCP ports and HTTP endpoints *you* list in `data/tools/mon.json`.
//!                     The model picks a probe by name and can never point one at an address.
//! * `mon.health`    – memory, disks and every probe in one report with ok/warn/critical.
//!
//! Log lines and event messages are other software's text: they are labelled untrusted, and the
//! host masks anything that looks like a credential before the model sees it.

use crate::cli::Cli;
use crate::fs::guard::{display, Op, PathGuard};
use aqua_tool_host::{
    Annotations, Availability, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    Tcp,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub name: String,
    pub kind: ProbeKind,
    pub host: String,
    pub port: u16,
    /// HTTP only: the path to request (default "/").
    #[serde(default)]
    pub path: Option<String>,
    /// HTTP only: use https.
    #[serde(default)]
    pub tls: bool,
    /// HTTP only: status codes that count as healthy (default 200-399).
    #[serde(default)]
    pub expect_status: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonConfig {
    #[serde(default)]
    pub probes: Vec<Probe>,
    #[serde(default = "ten")]
    pub disk_free_warn_percent: u64,
    #[serde(default = "ten")]
    pub memory_free_warn_percent: u64,
}

fn ten() -> u64 {
    10
}

impl Default for MonConfig {
    fn default() -> Self {
        Self { probes: Vec::new(), disk_free_warn_percent: 10, memory_free_warn_percent: 10 }
    }
}

impl MonConfig {
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.probes {
            if p.name.is_empty() || p.name.len() > 60 || !p.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                return Err(format!("probe name '{}' may only use letters, digits and - _ .", p.name));
            }
            if !seen.insert(p.name.clone()) {
                return Err(format!("probe '{}' is listed twice", p.name));
            }
            if p.host.is_empty() || p.host.len() > 253 || !p.host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':')) {
                return Err(format!("probe '{}': the host must be a plain name or address", p.name));
            }
            if p.port == 0 {
                return Err(format!("probe '{}': port 0 is not valid", p.name));
            }
            if let Some(path) = &p.path {
                if !path.starts_with('/') || path.len() > 300 || !path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '?' | '=' | '&' | '%' | ',' | ':')) {
                    return Err(format!("probe '{}': the path must start with / and use plain URL characters", p.name));
                }
            }
        }
        if self.disk_free_warn_percent > 90 || self.memory_free_warn_percent > 90 {
            return Err("warning thresholds must be 0-90 percent".into());
        }
        Ok(())
    }
}

struct Inner {
    cfg: MonConfig,
    guard: Arc<PathGuard>,
    wevtutil: Cli,
}

pub struct MonProvider(Arc<Inner>);

impl MonProvider {
    pub fn new(cfg: MonConfig, guard: Arc<PathGuard>, scratch: &Path) -> Result<Self, String> {
        cfg.validate()?;
        std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
        let wevtutil = Cli::find("wevtutil", &None, &["%SystemRoot%/System32"], scratch);
        Ok(Self(Arc::new(Inner { cfg, guard, wevtutil })))
    }

    /// Test hook: use a specific `wevtutil` program.
    pub fn with_wevtutil(cfg: MonConfig, guard: Arc<PathGuard>, scratch: &Path, wevtutil: &str) -> Result<Self, String> {
        cfg.validate()?;
        std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
        let wevtutil = Cli::find("wevtutil", &Some(wevtutil.to_string()), &[], scratch);
        Ok(Self(Arc::new(Inner { cfg, guard, wevtutil })))
    }
}

fn manifest(name: &str, description: &str, props: Value, required: &[&str], network: bool) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        classes: if network { vec![PermClass::Read, PermClass::Network] } else { vec![PermClass::Read] },
        default_decision: Decision::Auto,
        input_schema: json!({"type": "object", "properties": props, "required": required}),
        timeout_default_ms: 30_000,
        timeout_max_ms: 60_000,
        annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: network },
        untrusted_output: true,
    }
}

impl ToolProvider for MonProvider {
    fn id(&self) -> &str {
        "mon"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        vec![
            manifest(
                "mon.log_tail",
                "Read the last lines of a log file, optionally only lines containing some text. Files holding secrets are refused.",
                json!({"path": {"type": "string", "minLength": 1, "maxLength": 1024}, "lines": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100}, "contains": {"type": "string", "maxLength": 100}}),
                &["path"],
                false,
            ),
            manifest(
                "mon.eventlog",
                "Recent Windows Event Log entries from Application or System, newest first. Security is not available.",
                json!({"log": {"type": "string", "enum": ["Application", "System"], "default": "System"}, "minutes": {"type": "integer", "minimum": 1, "maximum": 1440, "default": 60},
                       "min_level": {"type": "string", "enum": ["error", "warning", "info"], "default": "warning"}, "source": {"type": "string", "maxLength": 80}, "max": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}}),
                &[],
                false,
            ),
            manifest("mon.probe", "Run one of the configured TCP or HTTP checks by name, or all of them. Reports up/down, status and time.", json!({"name": {"type": "string", "maxLength": 60}}), &[], true),
            manifest("mon.health", "One report: memory, disk space and every configured check, each marked ok, warn or critical.", json!({}), &[], true),
        ]
    }

    fn availability(&self, tool: &str) -> Availability {
        if tool == "mon.eventlog" && !self.0.wevtutil.installed() {
            return Availability::Unavailable("the Windows Event Log tool (wevtutil.exe) was not found".into());
        }
        Availability::Available
    }

    fn classify(&self, tool: &str, a: &Value) -> Result<Classification, ToolError> {
        let g = &self.0;
        match tool {
            "mon.log_tail" => {
                let r = g.check_log(a)?;
                Ok(Classification { classes: vec![PermClass::Read], target: display(&r), summary: format!("Read the end of {}", display(&r)), target_allowed: true })
            }
            "mon.eventlog" => {
                g.event_query(a)?;
                Ok(Classification { classes: vec![PermClass::Read], target: "Windows Event Log".into(), summary: "Read recent event log entries".into(), target_allowed: true })
            }
            "mon.probe" => {
                let n = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if !n.is_empty() {
                    g.probe_named(n)?;
                }
                Ok(Classification { classes: vec![PermClass::Read, PermClass::Network], target: if n.is_empty() { "all probes".into() } else { n.into() }, summary: "Run monitoring checks".into(), target_allowed: true })
            }
            "mon.health" => Ok(Classification { classes: vec![PermClass::Read, PermClass::Network], target: "this machine".into(), summary: "Health report".into(), target_allowed: true }),
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let inner = self.0.clone();
        let cancel = ctx.cancel.clone();
        let tool = tool.to_string();
        let timeout_ms = ctx.timeout_ms;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                // The programs of this call get no more than what is left of the host's time for it (card OA4-03).
                let _clock = crate::term::CallClock::start(timeout_ms);
                match tool.as_str() {
                    "mon.log_tail" => inner.log_tail(&args),
                    "mon.eventlog" => inner.eventlog(&args, &cancel),
                    "mon.probe" => inner.probe(&args),
                    "mon.health" => inner.health(),
                    other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
                }
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("monitor task failed: {e}")))?.map(ProviderOutput::marked_private)
        })
    }
}

const TAIL_WINDOW: u64 = 512 * 1024;
const MAX_LINE: usize = 2000;

/// The last `n` lines of `bytes` that contain `needle` (case-insensitive), each cut to a sane length.
/// `partial_first` drops the first line, which was cut in half by starting mid-file.
pub fn tail_lines(bytes: &[u8], n: usize, needle: &str, partial_first: bool) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if partial_first && !lines.is_empty() {
        lines.remove(0);
    }
    let needle = needle.to_lowercase();
    let mut kept: Vec<String> = lines
        .into_iter()
        .filter(|l| needle.is_empty() || l.to_lowercase().contains(&needle))
        .map(|l| if l.chars().count() > MAX_LINE { format!("{}…", l.chars().take(MAX_LINE).collect::<String>()) } else { l.to_string() })
        .collect();
    let start = kept.len().saturating_sub(n);
    kept.drain(..start);
    kept
}

impl Inner {
    fn check_log(&self, a: &Value) -> Result<std::path::PathBuf, ToolError> {
        let r = self.guard.resolve(a.get("path").and_then(|v| v.as_str()).unwrap_or(""), Op::Read)?;
        if r.sensitive {
            return Err(ToolError::new(ErrorCode::SecretBlocked, "that file looks like it holds secrets and is not readable through these tools"));
        }
        if !r.path.is_file() {
            return Err(ToolError::new(ErrorCode::NotFound, format!("{} is not a file", display(&r.path))));
        }
        Ok(r.path)
    }

    fn log_tail(&self, a: &Value) -> Result<ProviderOutput, ToolError> {
        let path = self.check_log(a)?;
        let n = a.get("lines").and_then(|v| v.as_u64()).unwrap_or(100).clamp(1, 500) as usize;
        let needle = a.get("contains").and_then(|v| v.as_str()).unwrap_or("");
        let mut f = std::fs::File::open(&path).map_err(|e| ToolError::new(ErrorCode::OsPermissionDenied, format!("{}: {e}", display(&path))))?;
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(TAIL_WINDOW);
        f.seek(SeekFrom::Start(start)).map_err(|e| ToolError::new(ErrorCode::BackendError, e.to_string()))?;
        let mut buf = Vec::new();
        f.take(TAIL_WINDOW).read_to_end(&mut buf).map_err(|e| ToolError::new(ErrorCode::BackendError, e.to_string()))?;
        let lines = tail_lines(&buf, n, needle, start > 0);
        let text = if lines.is_empty() { if needle.is_empty() { "The file is empty.".to_string() } else { format!("No recent lines contain '{needle}'.") } } else { lines.join("\n") };
        Ok(ProviderOutput {
            data: json!({"path": display(&path), "lines": lines.len(), "file_bytes": len, "searched_last_bytes": len.min(TAIL_WINDOW)}),
            text,
            warnings: if len > TAIL_WINDOW { vec![format!("only the last {} KB of a {} KB file were searched", TAIL_WINDOW / 1024, len / 1024)] } else { vec![] },
            ..Default::default()
        })
    }

    /// The arguments for `wevtutil qe`, after checking every input.
    fn event_query(&self, a: &Value) -> Result<Vec<String>, ToolError> {
        let bad = |m: &str| ToolError::new(ErrorCode::InvalidArguments, m.to_string());
        let log = match a.get("log").and_then(|v| v.as_str()).unwrap_or("System") {
            l @ ("Application" | "System") => l,
            "Security" => return Err(ToolError::new(ErrorCode::PermissionDenied, "the Security log is not available through these tools")),
            _ => return Err(bad("log must be Application or System")),
        };
        let minutes = a.get("minutes").and_then(|v| v.as_u64()).unwrap_or(60).clamp(1, 1440);
        let max = a.get("max").and_then(|v| v.as_u64()).unwrap_or(20).clamp(1, 100);
        let levels = match a.get("min_level").and_then(|v| v.as_str()).unwrap_or("warning") {
            "error" => "(Level=1 or Level=2)",
            "warning" => "(Level=1 or Level=2 or Level=3)",
            "info" => "(Level=1 or Level=2 or Level=3 or Level=4)",
            _ => return Err(bad("min_level must be error, warning or info")),
        };
        let mut inner = format!("{levels} and TimeCreated[timediff(@SystemTime) <= {}]", minutes * 60_000);
        if let Some(src) = a.get("source").and_then(|v| v.as_str()).filter(|x| !x.is_empty()) {
            if src.len() > 80 || !src.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-')) {
                return Err(bad("source may only use letters, digits, spaces and . _ -"));
            }
            inner.push_str(&format!(" and Provider[@Name='{src}']"));
        }
        let q = format!("*[System[{inner}]]");
        Ok(vec!["qe".into(), log.into(), format!("/q:{q}"), format!("/c:{max}"), "/rd:true".into(), "/f:text".into()])
    }

    fn eventlog(&self, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let args = self.event_query(a)?;
        let r = self.wevtutil.run(&args, &[], None, 30, 256, cancel)?;
        if r.code != Some(0) {
            let msg = r.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("wevtutil failed").trim().to_string();
            let code = if msg.to_lowercase().contains("access is denied") { ErrorCode::OsPermissionDenied } else { ErrorCode::BackendError };
            return Err(ToolError::new(code, msg));
        }
        let count = r.stdout.matches("Event[").count();
        let text = if count == 0 { "No matching events in that time.".to_string() } else { r.stdout.trim().to_string() };
        Ok(ProviderOutput { data: json!({"events": count}), text: if r.truncated { format!("{text}\n… output cut") } else { text }, ..Default::default() })
    }

    fn probe_named(&self, name: &str) -> Result<&Probe, ToolError> {
        self.cfg
            .probes
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("no probe named '{name}'. Configured: {}", self.cfg.probes.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "))))
    }

    fn run_probe(p: &Probe) -> Value {
        let started = Instant::now();
        let addr = (p.host.as_str(), p.port).to_socket_addrs().ok().and_then(|mut a| a.next());
        let Some(addr) = addr else {
            return json!({"probe": p.name, "up": false, "error": "the host name did not resolve"});
        };
        match p.kind {
            ProbeKind::Tcp => match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
                Ok(_) => json!({"probe": p.name, "up": true, "ms": started.elapsed().as_millis() as u64}),
                Err(e) => json!({"probe": p.name, "up": false, "error": e.kind().to_string()}),
            },
            ProbeKind::Http => {
                let url = format!("{}://{}:{}{}", if p.tls { "https" } else { "http" }, if p.host.contains(':') { format!("[{}]", p.host) } else { p.host.clone() }, p.port, p.path.as_deref().unwrap_or("/"));
                let agent = ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(5)).build();
                let status = match agent.get(&url).call() {
                    Ok(r) => Ok(r.status()),
                    Err(ureq::Error::Status(c, _)) => Ok(c),
                    Err(e) => Err(e.kind().to_string()),
                };
                match status {
                    Ok(code) => {
                        let healthy = if p.expect_status.is_empty() { (200..400).contains(&code) } else { p.expect_status.contains(&code) };
                        json!({"probe": p.name, "up": healthy, "status": code, "ms": started.elapsed().as_millis() as u64})
                    }
                    Err(e) => json!({"probe": p.name, "up": false, "error": e}),
                }
            }
        }
    }

    fn probe(&self, a: &Value) -> Result<ProviderOutput, ToolError> {
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let chosen: Vec<&Probe> = if name.is_empty() { self.cfg.probes.iter().collect() } else { vec![self.probe_named(name)?] };
        if chosen.is_empty() {
            return Ok(ProviderOutput { data: json!({"results": []}), text: "No checks are configured. Add them to data/tools/mon.json.".into(), ..Default::default() });
        }
        let results: Vec<Value> = chosen.into_iter().map(Self::run_probe).collect();
        Ok(ProviderOutput { text: probe_text(&results), data: json!({ "results": results }), ..Default::default() })
    }

    fn health(&self) -> Result<ProviderOutput, ToolError> {
        let (total, avail, drives) = crate::sys::snapshot();
        let mut lines = Vec::new();
        let mut worst = 0u8; // 0 ok, 1 warn, 2 critical
        let mem_pct = if total > 0 { avail * 100 / total } else { 100 };
        let mem_level = if mem_pct < self.cfg.memory_free_warn_percent / 2 { 2 } else if mem_pct < self.cfg.memory_free_warn_percent { 1 } else { 0 };
        worst = worst.max(mem_level);
        lines.push(format!("{} memory: {avail} MB free of {total} MB ({mem_pct}%)", label(mem_level)));
        let mut disk_rows = Vec::new();
        for (root, kind, t, f) in drives {
            if t == 0 || kind == "cdrom" {
                continue;
            }
            let pct = f * 100 / t;
            let lvl = if pct < self.cfg.disk_free_warn_percent / 2 { 2 } else if pct < self.cfg.disk_free_warn_percent { 1 } else { 0 };
            worst = worst.max(lvl);
            lines.push(format!("{} disk {root}: {} GB free of {} GB ({pct}%)", label(lvl), f >> 30, t >> 30));
            disk_rows.push(json!({"drive": root, "free_percent": pct, "level": label(lvl)}));
        }
        let results: Vec<Value> = self.cfg.probes.iter().map(Self::run_probe).collect();
        for r in &results {
            let up = r["up"].as_bool().unwrap_or(false);
            if !up {
                worst = 2;
            }
            lines.push(format!("{} check {}", if up { "ok      " } else { "CRITICAL" }, probe_text(std::slice::from_ref(r))));
        }
        let overall = label(worst).trim();
        Ok(ProviderOutput {
            data: json!({"overall": overall, "memory_free_percent": mem_pct, "disks": disk_rows, "checks": results}),
            text: format!("Overall: {overall}\n{}", lines.join("\n")),
            ..Default::default()
        })
    }
}

fn label(level: u8) -> &'static str {
    match level {
        0 => "ok      ",
        1 => "warn    ",
        _ => "critical",
    }
}

fn probe_text(results: &[Value]) -> String {
    results
        .iter()
        .map(|r| {
            let name = r["probe"].as_str().unwrap_or("");
            if r["up"].as_bool().unwrap_or(false) {
                format!("{name}: up{} ({} ms)", r.get("status").map(|s| format!(", status {s}")).unwrap_or_default(), r["ms"])
            } else {
                format!("{name}: DOWN ({})", r["error"].as_str().map(String::from).unwrap_or_else(|| format!("status {}", r["status"])))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines_filters_and_cuts_long_lines() {
        let text = (1..=10).map(|i| format!("line {i} {}", if i % 2 == 0 { "ERROR" } else { "ok" })).collect::<Vec<_>>().join("\n");
        assert_eq!(tail_lines(text.as_bytes(), 3, "", false), vec!["line 8 ERROR", "line 9 ok", "line 10 ERROR"]);
        assert_eq!(tail_lines(text.as_bytes(), 10, "error", false).len(), 5);
        assert_eq!(tail_lines(text.as_bytes(), 2, "ERROR", false), vec!["line 8 ERROR", "line 10 ERROR"]);
        assert_eq!(tail_lines(text.as_bytes(), 100, "", true).len(), 9, "the cut first line is dropped");
        assert!(tail_lines("x".repeat(5000).as_bytes(), 1, "", false)[0].chars().count() <= MAX_LINE + 1);
        assert_eq!(tail_lines(b"\xff\xfebad utf8\nok", 5, "", false).len(), 2, "invalid UTF-8 is tolerated");
    }

    #[test]
    fn config_validation_rejects_bad_probes() {
        let p = |n: &str, h: &str, port: u16, path: Option<&str>| Probe { name: n.into(), kind: ProbeKind::Http, host: h.into(), port, path: path.map(String::from), tls: false, expect_status: vec![] };
        assert!(MonConfig { probes: vec![p("web", "127.0.0.1", 80, Some("/health?x=1"))], ..Default::default() }.validate().is_ok());
        for bad in [p("", "h", 1, None), p("a b", "h", 1, None), p("a", "", 1, None), p("a", "h h", 1, None), p("a", "h/x", 1, None), p("a", "h", 0, None), p("a", "h", 1, Some("x")), p("a", "h", 1, Some("/a b")), p("a", "h", 1, Some("/\r\nHost: evil"))] {
            assert!(MonConfig { probes: vec![bad.clone()], ..Default::default() }.validate().is_err(), "{bad:?}");
        }
        assert!(MonConfig { probes: vec![p("a", "h", 1, None), p("a", "h", 2, None)], ..Default::default() }.validate().is_err());
        assert!(serde_json::from_str::<MonConfig>(r#"{"probes":[],"exec":"x"}"#).is_err());
    }

    #[test]
    fn labels_and_texts() {
        assert_eq!(label(0).trim(), "ok");
        assert_eq!(label(2).trim(), "critical");
        let t = probe_text(&[json!({"probe":"a","up":true,"status":200,"ms":3}), json!({"probe":"b","up":false,"error":"connection refused"})]);
        assert!(t.contains("a: up, status 200") && t.contains("b: DOWN (connection refused)"), "{t}");
    }
}
