//! Docker tools (`docker.*`) built on the `docker` command-line client.
//!
//! Docker is optional: without a `docker` program the tools are hidden, and with one installed but
//! its engine stopped, calls fail with `UNAVAILABLE` and say so. What Aqua creates is labelled
//! `aqua.managed=true`; starting, stopping, restarting, removing and exec-ing are limited to those
//! containers. `docker.run` builds a locked-down container: capabilities dropped, no privilege
//! escalation, memory, CPU and process limits, ports bound to 127.0.0.1 only, bind mounts only from
//! Aqua's allowed folders (read-only unless asked), no host network, no Docker socket. There is no
//! build, no prune, no privileged mode and no way to mount the Docker socket.

pub mod check;

use crate::contain::{build_env, Limits};
use crate::fs::guard::{display, Op, PathGuard};
use crate::term::run_command;
use aqua_tool_host::output::is_secret_key;
use aqua_tool_host::{
    Annotations, Availability, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerConfig {
    /// Path to `docker.exe`; found automatically when absent.
    #[serde(default)]
    pub docker_path: Option<String>,
    /// Also allow lifecycle changes on containers Aqua did not create. Off by default.
    #[serde(default)]
    pub manage_all: bool,
    /// Images that may be pulled without an extra prompt (`alpine`, `python:3.*`, `ghcr.io/org/*`).
    #[serde(default)]
    pub allow_images: Vec<String>,
    #[serde(default = "default_memory")]
    pub max_memory_mb: u64,
    #[serde(default = "default_cpus")]
    pub max_cpus: f64,
    #[serde(default = "default_port")]
    pub min_host_port: u16,
}

fn default_memory() -> u64 {
    4096
}
fn default_cpus() -> f64 {
    4.0
}
fn default_port() -> u16 {
    1024
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self { docker_path: None, manage_all: false, allow_images: Vec::new(), max_memory_mb: 4096, max_cpus: 4.0, min_host_port: 1024 }
    }
}

impl DockerConfig {
    pub fn validate(&self) -> Result<(), String> {
        for p in &self.allow_images {
            if p.is_empty() || p == "*" || p.contains(char::is_whitespace) {
                return Err(format!("allow_images entry '{p}' must name an image or a specific pattern, not '*'"));
            }
        }
        if self.max_memory_mb < 64 || self.max_memory_mb > 65_536 || self.max_cpus < 0.1 || self.max_cpus > 64.0 || self.min_host_port < 1 {
            return Err("max_memory_mb must be 64-65536, max_cpus 0.1-64 and min_host_port at least 1".into());
        }
        Ok(())
    }
}

struct Inner {
    cfg: DockerConfig,
    guard: Arc<PathGuard>,
    docker: Option<PathBuf>,
    scratch: PathBuf,
}

/// The Docker tools. State is behind an `Arc` so blocking tasks can own it.
pub struct DockerProvider(Arc<Inner>);

impl DockerProvider {
    pub fn new(cfg: DockerConfig, guard: Arc<PathGuard>, scratch: &Path) -> Result<Self, String> {
        cfg.validate()?;
        std::fs::create_dir_all(scratch).map_err(|e| format!("cannot create {}: {e}", scratch.display()))?;
        let docker = find_docker(&cfg.docker_path);
        Ok(Self(Arc::new(Inner { cfg, guard, docker, scratch: scratch.to_path_buf() })))
    }

    /// Whether a `docker` program was found. The tools are hidden when it was not.
    pub fn installed(&self) -> bool {
        self.0.docker.is_some()
    }
}

fn find_docker(configured: &Option<String>) -> Option<PathBuf> {
    if let Some(p) = configured {
        let p = PathBuf::from(crate::fs::guard::expand_env(p));
        return p.is_file().then_some(p);
    }
    for base in ["ProgramFiles", "ProgramW6432"] {
        if let Ok(b) = std::env::var(base) {
            let p = PathBuf::from(b).join("Docker/Docker/resources/bin/docker.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("docker.exe")).find(|p| p.is_file())
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, msg)
}

fn arg_str<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn arg_bool(a: &Value, k: &str) -> bool {
    a.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn arg_strs(a: &Value, k: &str) -> Vec<String> {
    a.get(k).and_then(|v| v.as_array()).map(|x| x.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Lifecycle,
    Remove,
    Pull,
    Run,
    Exec,
    ComposeUp,
    ComposeDown,
}

fn specs() -> Vec<(&'static str, &'static str, Kind, Value, Vec<&'static str>)> {
    let c = json!({"type": "string", "minLength": 1, "maxLength": 128});
    let file = json!({"type": "string", "minLength": 1, "maxLength": 1024});
    let project = json!({"type": "string", "maxLength": 63});
    let services = json!({"type": "array", "items": {"type": "string", "maxLength": 63}, "maxItems": 20});
    vec![
        ("docker.ps", "List containers (running only, or all with all=true) and whether Aqua created them.", Kind::Read, json!({"all": {"type": "boolean", "default": false}}), vec![]),
        ("docker.images", "List local images.", Kind::Read, json!({}), vec![]),
        ("docker.inspect", "Summarise one container: image, state, ports, mounts, limits and environment variable names (not values).", Kind::Read, json!({"container": c.clone()}), vec!["container"]),
        ("docker.logs", "Read the last lines of a container's log.", Kind::Read, json!({"container": c.clone(), "tail": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100}, "since": {"type": "string", "maxLength": 30}}), vec!["container"]),
        ("docker.stats", "One reading of CPU and memory use, for one container or all running ones.", Kind::Read, json!({"container": c.clone()}), vec![]),
        ("docker.networks", "List Docker networks.", Kind::Read, json!({}), vec![]),
        ("docker.volumes", "List Docker volumes.", Kind::Read, json!({}), vec![]),
        ("docker.compose_ps", "List the containers of a compose project.", Kind::Read, json!({"file": file.clone(), "project": project.clone()}), vec!["file"]),
        ("docker.compose_logs", "Read the last lines of a compose project's logs.", Kind::Read, json!({"file": file.clone(), "project": project.clone(), "services": services.clone(), "tail": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100}}), vec!["file"]),
        ("docker.start", "Start a stopped container that Aqua created.", Kind::Lifecycle, json!({"container": c.clone()}), vec!["container"]),
        ("docker.stop", "Stop a running container that Aqua created.", Kind::Lifecycle, json!({"container": c.clone(), "timeout_s": {"type": "integer", "minimum": 1, "maximum": 120, "default": 10}}), vec!["container"]),
        ("docker.restart", "Restart a container that Aqua created.", Kind::Lifecycle, json!({"container": c.clone(), "timeout_s": {"type": "integer", "minimum": 1, "maximum": 120, "default": 10}}), vec!["container"]),
        ("docker.pull", "Download an image. Images not on the allow list ask first.", Kind::Pull, json!({"image": {"type": "string", "minLength": 1, "maxLength": 255}}), vec!["image"]),
        ("docker.run", "Start a locked-down container from a local image: limits, no privileges, ports on 127.0.0.1, mounts only from allowed folders. Asks.", Kind::Run,
            json!({
                "image": {"type": "string", "minLength": 1, "maxLength": 255},
                "name": {"type": "string", "maxLength": 128},
                "command": {"type": "array", "items": {"type": "string", "maxLength": 2048}, "maxItems": 64},
                "env": {"type": "object", "additionalProperties": true},
                "ports": {"type": "array", "maxItems": 10, "items": {"type": "object", "additionalProperties": true}},
                "mounts": {"type": "array", "maxItems": 10, "items": {"type": "object", "additionalProperties": true}},
                "memory_mb": {"type": "integer", "minimum": 16, "maximum": 65536, "default": 512},
                "cpus": {"type": "number", "minimum": 0.1, "maximum": 64, "default": 1},
                "network": {"type": "string", "enum": ["bridge", "none"], "default": "bridge"},
                "read_only": {"type": "boolean", "default": false},
                "user": {"type": "string", "maxLength": 64},
                "workdir": {"type": "string", "maxLength": 512},
                "remove_when_stopped": {"type": "boolean", "default": false}
            }), vec!["image"]),
        ("docker.exec", "Run a command inside a container that Aqua created. Always asks.", Kind::Exec, json!({"container": c.clone(), "command": {"type": "array", "minItems": 1, "maxItems": 64, "items": {"type": "string", "maxLength": 2048}}, "workdir": {"type": "string", "maxLength": 512}, "timeout_s": {"type": "integer", "minimum": 1, "maximum": 300, "default": 60}}), vec!["container", "command"]),
        ("docker.rm", "Remove a stopped container that Aqua created (force=true also stops a running one). Volumes are kept. Always asks.", Kind::Remove, json!({"container": c, "force": {"type": "boolean", "default": false}}), vec!["container"]),
        ("docker.rmi", "Remove a local image that no container uses. Always asks.", Kind::Remove, json!({"image": {"type": "string", "minLength": 1, "maxLength": 255}}), vec!["image"]),
        ("docker.compose_up", "Start a compose project from a file in an allowed folder. Refuses privileges, host access and builds. Always asks.", Kind::ComposeUp, json!({"file": file.clone(), "project": project.clone(), "services": services.clone()}), vec!["file"]),
        ("docker.compose_down", "Stop and remove a compose project's containers and networks (volumes are kept). Always asks.", Kind::ComposeDown, json!({"file": file, "project": project}), vec!["file"]),
    ]
}

impl ToolProvider for DockerProvider {
    fn id(&self) -> &str {
        "docker"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        specs()
            .into_iter()
            .map(|(name, description, kind, props, required)| {
                let classes = match kind {
                    Kind::Read => vec![PermClass::Read],
                    Kind::Lifecycle => vec![PermClass::Write],
                    Kind::Remove | Kind::ComposeDown => vec![PermClass::Write, PermClass::Destructive],
                    Kind::Pull => vec![PermClass::Network, PermClass::Write],
                    Kind::Run | Kind::Exec => vec![PermClass::Execute],
                    Kind::ComposeUp => vec![PermClass::Execute, PermClass::ExternalEffect],
                };
                let slow = matches!(kind, Kind::Pull | Kind::ComposeUp | Kind::Exec);
                ToolManifest {
                    name: name.into(),
                    title: name.into(),
                    description: description.into(),
                    classes,
                    default_decision: Decision::Auto,
                    input_schema: json!({"type": "object", "properties": props, "required": required}),
                    timeout_default_ms: if slow { 330_000 } else { 60_000 },
                    timeout_max_ms: if slow { 330_000 } else { 90_000 },
                    annotations: Annotations { read_only: kind == Kind::Read, destructive: matches!(kind, Kind::Remove | Kind::ComposeDown), idempotent: kind == Kind::Read, open_world: matches!(kind, Kind::Pull | Kind::Run) },
                    // Logs, image names and container output come from other people's software.
                    untrusted_output: true,
                }
            })
            .collect()
    }

    fn availability(&self, _tool: &str) -> Availability {
        if self.0.docker.is_none() {
            return Availability::Unavailable("Docker is not installed (no docker.exe found). Install Docker Desktop, or set docker_path in data/tools/docker.json.".into());
        }
        Availability::Available
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let s = &self.0;
        let (_, _, kind, _, _) = specs().into_iter().find(|x| x.0 == tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        match kind {
            Kind::Read => {
                if let Some(c) = args.get("container").and_then(|v| v.as_str()).filter(|c| !c.is_empty()) {
                    check::container_ref(c)?;
                }
                if tool.starts_with("docker.compose") {
                    s.compose_file(args)?;
                }
                Ok(Classification { classes: vec![PermClass::Read], target: "docker".into(), summary: tool.into(), target_allowed: true })
            }
            Kind::Lifecycle | Kind::Remove if tool != "docker.rmi" => {
                let c = check::container_ref(arg_str(args, "container"))?;
                s.require_managed(c)?;
                let classes = if kind == Kind::Remove { vec![PermClass::Write, PermClass::Destructive] } else { vec![PermClass::Write] };
                let verb = tool.trim_start_matches("docker.");
                Ok(Classification { classes, target: c.to_string(), summary: format!("{} container {c}{}", capitalise(verb), if arg_bool(args, "force") { " (force)" } else { "" }), target_allowed: true })
            }
            Kind::Remove => {
                let img = check::image_ref(arg_str(args, "image"))?;
                Ok(Classification { classes: vec![PermClass::Write, PermClass::Destructive], target: img.to_string(), summary: format!("Remove image {img}"), target_allowed: false })
            }
            Kind::Pull => {
                let img = check::image_ref(arg_str(args, "image"))?;
                let allowed = s.cfg.allow_images.iter().any(|p| check::image_matches(p, img));
                Ok(Classification { classes: vec![PermClass::Network, PermClass::Write], target: img.to_string(), summary: format!("Pull image {img}{}", if allowed { "" } else { " (not on the allow list)" }), target_allowed: allowed })
            }
            Kind::Run => {
                let plan = s.run_plan(args)?;
                let mut classes = vec![PermClass::Execute];
                if plan.network != "none" {
                    classes.push(PermClass::Network);
                }
                if plan.writable_mount {
                    classes.push(PermClass::Write);
                }
                Ok(Classification { classes, target: plan.image.clone(), summary: plan.summary(), target_allowed: false })
            }
            Kind::Exec => {
                let c = check::container_ref(arg_str(args, "container"))?;
                s.require_managed(c)?;
                let cmd = arg_strs(args, "command");
                if cmd.iter().any(|x| x.contains('\0')) {
                    return Err(bad("the command contains a NUL character"));
                }
                Ok(Classification { classes: vec![PermClass::Execute], target: c.to_string(), summary: format!("Run `{}` inside {c}", truncate(&cmd.join(" "), 300)), target_allowed: false })
            }
            Kind::ComposeUp | Kind::ComposeDown => {
                let (file, project) = s.compose_file(args)?;
                let classes = if kind == Kind::ComposeUp { vec![PermClass::Execute, PermClass::ExternalEffect] } else { vec![PermClass::Write, PermClass::Destructive] };
                Ok(Classification { classes, target: project.clone(), summary: format!("{} compose project {project} from {}", if kind == Kind::ComposeUp { "Start" } else { "Stop and remove" }, display(&file)), target_allowed: false })
            }
            Kind::Lifecycle => unreachable!("handled above"),
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
                inner.dispatch(&tool, &args, &cancel)
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("docker task failed: {e}")))?.map(ProviderOutput::marked_private)
        })
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

struct RunPlan {
    image: String,
    args: Vec<String>,
    network: String,
    writable_mount: bool,
    ports: Vec<String>,
    mounts: Vec<String>,
    memory_mb: u64,
    cpus: f64,
    command: Vec<String>,
}

impl RunPlan {
    fn summary(&self) -> String {
        let mut s = format!("Start container from {} ({} MB, {} CPU, network {})", self.image, self.memory_mb, self.cpus, self.network);
        if !self.ports.is_empty() {
            s.push_str(&format!("; ports {}", self.ports.join(", ")));
        }
        if !self.mounts.is_empty() {
            s.push_str(&format!("; mounts {}", self.mounts.join(", ")));
        }
        if !self.command.is_empty() {
            s.push_str(&format!("; command `{}`", truncate(&self.command.join(" "), 200)));
        }
        s
    }
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
}

impl Inner {
    fn docker_path(&self) -> Result<&PathBuf, ToolError> {
        self.docker.as_ref().ok_or_else(|| ToolError::new(ErrorCode::Unavailable, "Docker is not installed"))
    }

    fn run(&self, args: &[String], timeout_s: u64, cap_kb: usize, cancel: &AtomicBool) -> Result<Ran, ToolError> {
        let exe = self.docker_path()?.clone();
        // Docker's own settings (context, host, credentials) come from its config folder, not from Aqua's environment.
        let r = run_command(exe, args.to_vec(), self.scratch.clone(), build_env(std::env::vars(), &HashMap::new()).unwrap_or_default(), None, Limits { memory_mb: 512, max_processes: 16 }, Duration::from_secs(timeout_s), cap_kb * 1024, cancel)?;
        if r.cancelled {
            return Err(ToolError::new(ErrorCode::Cancelled, "the run was cancelled and docker was stopped"));
        }
        if r.timed_out {
            return Err(ToolError::new(ErrorCode::Timeout, format!("docker did not finish within {} and was stopped", r.limit_words())));
        }
        Ok(Ran { code: r.exit_code, stdout: r.stdout, stderr: r.stderr, truncated: r.truncated })
    }

    fn ok(&self, args: &[String], timeout_s: u64, cap_kb: usize, cancel: &AtomicBool) -> Result<Ran, ToolError> {
        let r = self.run(args, timeout_s, cap_kb, cancel)?;
        if r.code == Some(0) {
            Ok(r)
        } else {
            Err(docker_failure(&r.stderr, &r.stdout))
        }
    }

    /// Labels of a container, or an error when it does not exist or the engine is down.
    fn labels(&self, container: &str) -> Result<Value, ToolError> {
        let cancel = AtomicBool::new(false);
        let r = self.ok(&s(&["container", "inspect", "-f", "{{json .Config.Labels}}", container]), 20, 16, &cancel)?;
        Ok(serde_json::from_str(r.stdout.trim()).unwrap_or(Value::Null))
    }

    fn require_managed(&self, container: &str) -> Result<(), ToolError> {
        if self.cfg.manage_all {
            return Ok(());
        }
        let labels = self.labels(container)?;
        if labels.get("aqua.managed").and_then(|v| v.as_str()) == Some("true") {
            Ok(())
        } else {
            Err(ToolError::new(ErrorCode::PermissionDenied, format!("container '{container}' was not created by Aqua, so it is not changed through these tools (set manage_all in data/tools/docker.json to allow that)")))
        }
    }

    /// The compose file (inside the allowed folders, scanned) and the project name.
    fn compose_file(&self, a: &Value) -> Result<(PathBuf, String), ToolError> {
        let r = self.guard.resolve(arg_str(a, "file"), Op::Read)?;
        let low = r.path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if !(low.ends_with(".yml") || low.ends_with(".yaml")) {
            return Err(bad("the compose file must be a .yml or .yaml file"));
        }
        let text = std::fs::read_to_string(&r.path).map_err(|e| ToolError::new(ErrorCode::NotFound, format!("{}: {e}", display(&r.path))))?;
        if text.len() > 512 * 1024 {
            return Err(ToolError::new(ErrorCode::ResourceLimit, "the compose file is larger than 512 KB"));
        }
        let problems = check::scan_compose(&text);
        if !problems.is_empty() {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("the compose file asks for things Aqua does not allow: {}", problems.join("; "))).with_details(json!({"problems": problems})));
        }
        let project = match arg_str(a, "project") {
            "" => {
                let dir = r.path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                let cleaned: String = dir.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
                check::project_name(cleaned.trim_start_matches(['-', '_'])).map_err(|_| bad("give a project name"))?.to_string()
            }
            p => check::project_name(p)?.to_string(),
        };
        for svc in arg_strs(a, "services") {
            check::project_name(&svc.to_ascii_lowercase()).map_err(|_| bad(format!("'{svc}' is not a valid service name")))?;
        }
        Ok((r.path, project))
    }

    fn run_plan(&self, a: &Value) -> Result<RunPlan, ToolError> {
        let image = check::image_ref(arg_str(a, "image"))?.to_string();
        let mut args: Vec<String> = s(&["run", "-d", "--pull", "never", "--label", "aqua.managed=true", "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true", "--pids-limit", "256"]);
        let memory_mb = a.get("memory_mb").and_then(|v| v.as_u64()).unwrap_or(512).min(self.cfg.max_memory_mb);
        let cpus = a.get("cpus").and_then(|v| v.as_f64()).unwrap_or(1.0).min(self.cfg.max_cpus);
        args.extend(s(&["--memory", &format!("{memory_mb}m"), "--memory-swap", &format!("{memory_mb}m"), "--cpus", &format!("{cpus}")]));
        let network = match arg_str(a, "network") {
            "" | "bridge" => "bridge".to_string(),
            "none" => "none".to_string(),
            other => return Err(bad(format!("network '{other}' is not allowed (bridge or none)"))),
        };
        args.extend(s(&["--network", &network]));
        if let Some(n) = a.get("name").and_then(|v| v.as_str()).filter(|n| !n.is_empty()) {
            args.extend(s(&["--name", check::container_ref(n)?]));
        }
        if arg_bool(a, "read_only") {
            args.push("--read-only".into());
        }
        if arg_bool(a, "remove_when_stopped") {
            args.push("--rm".into());
        }
        if let Some(u) = a.get("user").and_then(|v| v.as_str()).filter(|u| !u.is_empty()) {
            if !u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.')) || u.starts_with('-') || u == "root" || u == "0" || u.starts_with("0:") {
                return Err(bad("user must be a non-root user name or numeric id"));
            }
            args.extend(s(&["--user", u]));
        }
        if let Some(w) = a.get("workdir").and_then(|v| v.as_str()).filter(|w| !w.is_empty()) {
            args.extend(s(&["--workdir", check::container_path(w)?]));
        }
        if let Some(env) = a.get("env").and_then(|v| v.as_object()) {
            for (k, v) in env {
                check::env_name(k)?;
                if is_secret_key(k) || ["KEY", "PASS", "PWD"].iter().any(|w| k.to_ascii_uppercase().contains(w)) {
                    return Err(ToolError::new(ErrorCode::SecretBlocked, format!("'{k}' looks like a secret. Secrets are not passed to containers through the model.")));
                }
                let val = match v {
                    Value::String(x) => x.clone(),
                    other => other.to_string(),
                };
                if val.contains(['\0', '\n', '\r']) || val.len() > 2000 {
                    return Err(bad(format!("the value of '{k}' is too long or contains control characters")));
                }
                args.extend(s(&["-e", &format!("{k}={val}")]));
            }
        }
        let mut ports = Vec::new();
        for p in a.get("ports").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let host = p.get("host").and_then(|v| v.as_u64()).unwrap_or(0);
            let container = p.get("container").and_then(|v| v.as_u64()).unwrap_or(0);
            let proto = p.get("protocol").and_then(|v| v.as_str()).unwrap_or("tcp");
            if host < self.cfg.min_host_port as u64 || host > 65535 || container == 0 || container > 65535 || !matches!(proto, "tcp" | "udp") {
                return Err(bad(format!("a port needs host {}-65535, container 1-65535 and protocol tcp or udp", self.cfg.min_host_port)));
            }
            // Always on the loopback address: a published port is never reachable from the network.
            let spec = format!("127.0.0.1:{host}:{container}/{proto}");
            args.extend(s(&["-p", &spec]));
            ports.push(spec);
        }
        let mut mounts = Vec::new();
        let mut writable_mount = false;
        for m in a.get("mounts").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let source = m.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let target = check::container_path(m.get("target").and_then(|v| v.as_str()).unwrap_or(""))?;
            let readonly = m.get("readonly").and_then(|v| v.as_bool()).unwrap_or(true);
            let r = self.guard.resolve(source, if readonly { Op::Read } else { Op::Write })?;
            let path = r.path.to_string_lossy().to_string();
            if path.contains([',', '"', '\n', '\r']) {
                return Err(bad("a mounted path may not contain commas or quotes"));
            }
            if !readonly {
                writable_mount = true;
            }
            args.extend(s(&["--mount", &format!("type=bind,source={path},target={target}{}", if readonly { ",readonly" } else { "" })]));
            mounts.push(format!("{}→{target}{}", display(&r.path), if readonly { " (read-only)" } else { " (read-write)" }));
        }
        let command = arg_strs(a, "command");
        if command.iter().any(|c| c.contains('\0')) {
            return Err(bad("the command contains a NUL character"));
        }
        args.push("--".into());
        args.push(image.clone());
        args.extend(command.iter().cloned());
        Ok(RunPlan { image, args, network, writable_mount, ports, mounts, memory_mb, cpus, command })
    }

    fn dispatch(&self, tool: &str, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let out = |data: Value, text: String, r: &Ran| Ok(ProviderOutput { data, text: if r.truncated { format!("{text}\n… output cut") } else { text }, ..Default::default() });
        match tool {
            "docker.ps" => {
                let mut v = s(&["ps", "--no-trunc", "--format", "{{json .}}"]);
                if arg_bool(a, "all") {
                    v.insert(1, "-a".into());
                }
                let r = self.ok(&v, 30, 128, cancel)?;
                let rows: Vec<Value> = json_lines(&r.stdout)
                    .into_iter()
                    .map(|c| {
                        let managed = c["Labels"].as_str().unwrap_or("").contains("aqua.managed=true");
                        json!({"id": short(c["ID"].as_str().unwrap_or("")), "names": c["Names"], "image": c["Image"], "state": c["State"], "status": c["Status"], "ports": c["Ports"], "aqua_managed": managed})
                    })
                    .collect();
                let text = rows.iter().map(|c| format!("{} {} {} {}{}", c["id"].as_str().unwrap_or(""), c["names"].as_str().unwrap_or(""), c["image"].as_str().unwrap_or(""), c["status"].as_str().unwrap_or(""), if c["aqua_managed"] == true { " [Aqua]" } else { "" })).collect::<Vec<_>>().join("\n");
                out(json!({ "containers": rows }), if text.is_empty() { "No containers.".into() } else { text }, &r)
            }
            "docker.images" => {
                let r = self.ok(&s(&["images", "--format", "{{json .}}"]), 30, 128, cancel)?;
                let rows: Vec<Value> = json_lines(&r.stdout).into_iter().map(|i| json!({"repository": i["Repository"], "tag": i["Tag"], "id": i["ID"], "size": i["Size"], "created": i["CreatedSince"]})).collect();
                let text = rows.iter().map(|i| format!("{}:{} {} {}", i["repository"].as_str().unwrap_or(""), i["tag"].as_str().unwrap_or(""), i["size"].as_str().unwrap_or(""), i["created"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n");
                out(json!({ "images": rows }), if text.is_empty() { "No images.".into() } else { text }, &r)
            }
            "docker.networks" | "docker.volumes" => {
                let what = if tool == "docker.networks" { "network" } else { "volume" };
                let r = self.ok(&s(&[what, "ls", "--format", "{{json .}}"]), 30, 64, cancel)?;
                let rows = json_lines(&r.stdout);
                let text = rows.iter().map(|x| format!("{} ({})", x["Name"].as_str().unwrap_or(""), x["Driver"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n");
                out(json!({ "items": rows }), if text.is_empty() { format!("No {what}s.") } else { text }, &r)
            }
            "docker.inspect" => {
                let c = check::container_ref(arg_str(a, "container"))?;
                let r = self.ok(&s(&["container", "inspect", c]), 30, 256, cancel)?;
                let v: Value = serde_json::from_str(&r.stdout).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("docker inspect did not return JSON: {e}")))?;
                let summary = summarise_inspect(&v[0]);
                let text = format!("{} ({}) image {} state {}{}{}", summary["name"].as_str().unwrap_or(""), short(summary["id"].as_str().unwrap_or("")), summary["image"].as_str().unwrap_or(""), summary["state"].as_str().unwrap_or(""), if summary["privileged"] == true { " PRIVILEGED" } else { "" }, if summary["aqua_managed"] == true { " [Aqua]" } else { "" });
                out(summary, text, &r)
            }
            "docker.logs" => {
                let c = check::container_ref(arg_str(a, "container"))?;
                let mut v = s(&["logs", "--tail", &a.get("tail").and_then(|x| x.as_u64()).unwrap_or(100).min(1000).to_string()]);
                let since = arg_str(a, "since");
                if !since.is_empty() {
                    if since.starts_with('-') || !since.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | 'T' | 'Z')) {
                        return Err(bad("since must be a duration like 10m or a timestamp"));
                    }
                    v.extend(s(&["--since", since]));
                }
                v.push(c.into());
                let r = self.ok(&v, 30, 128, cancel)?;
                // docker writes a container's stderr to stderr, so both belong in the log.
                let text = format!("{}{}", r.stdout, r.stderr).trim_end().to_string();
                out(json!({"container": c, "lines": text.lines().count()}), if text.is_empty() { "(no log output)".into() } else { text }, &r)
            }
            "docker.stats" => {
                let mut v = s(&["stats", "--no-stream", "--format", "{{json .}}"]);
                if let Some(c) = a.get("container").and_then(|x| x.as_str()).filter(|c| !c.is_empty()) {
                    v.push(check::container_ref(c)?.into());
                }
                let r = self.ok(&v, 45, 64, cancel)?;
                let rows = json_lines(&r.stdout);
                let text = rows.iter().map(|x| format!("{} cpu {} mem {}", x["Name"].as_str().unwrap_or(""), x["CPUPerc"].as_str().unwrap_or(""), x["MemUsage"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n");
                out(json!({ "stats": rows }), if text.is_empty() { "No running containers.".into() } else { text }, &r)
            }
            "docker.start" | "docker.stop" | "docker.restart" => {
                let c = check::container_ref(arg_str(a, "container"))?;
                self.require_managed(c)?;
                let verb = tool.trim_start_matches("docker.");
                let mut v = s(&[verb]);
                if verb != "start" {
                    v.extend(s(&["-t", &a.get("timeout_s").and_then(|x| x.as_u64()).unwrap_or(10).clamp(1, 120).to_string()]));
                }
                v.push(c.into());
                let r = self.ok(&v, 150, 8, cancel)?;
                out(json!({"container": c, "action": verb}), format!("{} {c}.", capitalise(&format!("{verb}{}", if verb == "stop" { "ped" } else if verb == "start" { "ed" } else { "ed" }))), &r)
            }
            "docker.rm" => {
                let c = check::container_ref(arg_str(a, "container"))?;
                self.require_managed(c)?;
                let mut v = s(&["rm"]);
                if arg_bool(a, "force") {
                    v.push("-f".into());
                }
                v.push(c.into());
                let r = self.ok(&v, 60, 8, cancel)?;
                out(json!({"container": c, "removed": true}), format!("Removed container {c}. Its volumes were kept."), &r)
            }
            "docker.rmi" => {
                let img = check::image_ref(arg_str(a, "image"))?;
                let r = self.ok(&s(&["rmi", img]), 60, 16, cancel)?;
                out(json!({"image": img}), format!("Removed image {img}."), &r)
            }
            "docker.pull" => {
                let img = check::image_ref(arg_str(a, "image"))?;
                let r = self.ok(&s(&["pull", "--quiet", img]), 300, 32, cancel)?;
                out(json!({"image": img}), format!("Pulled {img}. {}", r.stdout.trim()), &r)
            }
            "docker.run" => {
                let plan = self.run_plan(a)?;
                let r = self.ok(&plan.args, 120, 16, cancel)?;
                let id = r.stdout.lines().last().unwrap_or("").trim().to_string();
                out(json!({"container_id": id, "image": plan.image, "ports": plan.ports, "mounts": plan.mounts}), format!("Started container {} from {}.", short(&id), plan.image), &r)
            }
            "docker.exec" => {
                let c = check::container_ref(arg_str(a, "container"))?;
                self.require_managed(c)?;
                let mut v = s(&["exec"]);
                if let Some(w) = a.get("workdir").and_then(|x| x.as_str()).filter(|w| !w.is_empty()) {
                    v.extend(s(&["--workdir", check::container_path(w)?]));
                }
                v.push(c.into());
                v.extend(arg_strs(a, "command"));
                let timeout = a.get("timeout_s").and_then(|x| x.as_u64()).unwrap_or(60).clamp(1, 300);
                let r = self.run(&v, timeout, 128, cancel)?;
                if r.code.is_none() || r.code == Some(125) || r.code == Some(126) && r.stdout.is_empty() && r.stderr.contains("docker") {
                    return Err(docker_failure(&r.stderr, &r.stdout));
                }
                let mut text = format!("exit code {}", r.code.map(|c| c.to_string()).unwrap_or_default());
                if !r.stdout.is_empty() {
                    text.push_str(&format!("\n--- stdout ---\n{}", r.stdout));
                }
                if !r.stderr.is_empty() {
                    text.push_str(&format!("\n--- stderr ---\n{}", r.stderr));
                }
                out(json!({"container": c, "exit_code": r.code, "stdout": r.stdout, "stderr": r.stderr}), text, &r)
            }
            "docker.compose_ps" | "docker.compose_logs" | "docker.compose_up" | "docker.compose_down" => {
                let (file, project) = self.compose_file(a)?;
                let mut v = s(&["compose", "-f", &file.to_string_lossy(), "-p", &project]);
                let services: Vec<String> = arg_strs(a, "services");
                match tool {
                    "docker.compose_ps" => v.extend(s(&["ps", "--all"])),
                    "docker.compose_logs" => {
                        v.extend(s(&["logs", "--no-color", "--tail", &a.get("tail").and_then(|x| x.as_u64()).unwrap_or(100).min(1000).to_string()]));
                        v.extend(services);
                    }
                    // No building and no implicit pulling: the images must already be here.
                    "docker.compose_up" => {
                        v.extend(s(&["up", "-d", "--no-build", "--pull", "never"]));
                        v.extend(services);
                    }
                    _ => v.extend(s(&["down", "--remove-orphans"])),
                }
                let slow = matches!(tool, "docker.compose_up" | "docker.compose_down");
                let r = self.ok(&v, if slow { 300 } else { 60 }, 128, cancel)?;
                let text = format!("{}{}", r.stdout, r.stderr).trim().to_string();
                out(json!({"project": project, "file": display(&file)}), if text.is_empty() { "Done.".into() } else { text }, &r)
            }
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn short(id: &str) -> String {
    id.trim_start_matches("sha256:").chars().take(12).collect()
}

fn json_lines(text: &str) -> Vec<Value> {
    text.lines().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).collect()
}

/// A short, safe description of `docker inspect` output. Environment values are dropped: they routinely hold secrets.
fn summarise_inspect(c: &Value) -> Value {
    let env_names: Vec<String> = c["Config"]["Env"].as_array().map(|a| a.iter().filter_map(|e| e.as_str()).map(|e| e.split('=').next().unwrap_or("").to_string()).collect()).unwrap_or_default();
    let mounts: Vec<Value> = c["Mounts"].as_array().map(|a| a.iter().map(|m| json!({"type": m["Type"], "source": m["Source"], "destination": m["Destination"], "read_write": m["RW"]})).collect()).unwrap_or_default();
    let labels = c["Config"]["Labels"].as_object();
    json!({
        "id": c["Id"],
        "name": c["Name"].as_str().unwrap_or("").trim_start_matches('/'),
        "image": c["Config"]["Image"],
        "state": c["State"]["Status"],
        "started_at": c["State"]["StartedAt"],
        "exit_code": c["State"]["ExitCode"],
        "privileged": c["HostConfig"]["Privileged"],
        "network_mode": c["HostConfig"]["NetworkMode"],
        "memory_bytes": c["HostConfig"]["Memory"],
        "ports": c["NetworkSettings"]["Ports"],
        "mounts": mounts,
        "env_names": env_names,
        "aqua_managed": labels.and_then(|l| l.get("aqua.managed")).and_then(|v| v.as_str()) == Some("true"),
    })
}

fn docker_failure(stderr: &str, stdout: &str) -> ToolError {
    let all = format!("{stderr}\n{stdout}");
    let low = all.to_ascii_lowercase();
    let first = all.lines().find(|l| !l.trim().is_empty()).unwrap_or("docker failed").trim().to_string();
    let has = |m: &str| low.contains(m);
    let code = if has("cannot connect to the docker daemon") || has("error during connect") || has("is the docker daemon running") || has("docker daemon is not running") || has("pipe/docker") && has("cannot find the file") {
        return ToolError::new(ErrorCode::Unavailable, "Docker is installed but its engine is not running. Start Docker Desktop and try again.");
    } else if has("no such container") || has("no such image") || has("no such object") || has("manifest unknown") || has("pull access denied") || has("repository does not exist") || has("not found") && !has("command not found") {
        ErrorCode::NotFound
    } else if has("already in use") || has("already exists") {
        ErrorCode::AlreadyExists
    } else if has("port is already allocated") || has("address already in use") || has("is not running") || has("is already running") || has("conflict") {
        ErrorCode::Conflict
    } else if has("permission denied") || has("access is denied") {
        ErrorCode::OsPermissionDenied
    } else if has("no matching manifest") || has("unsupported") {
        ErrorCode::UnsupportedPlatform
    } else {
        ErrorCode::BackendError
    };
    ToolError::new(code, first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_rejects_wildcard_image_allowances_and_silly_limits() {
        assert!(DockerConfig::default().validate().is_ok());
        for bad in ["*", "", "a b"] {
            assert!(DockerConfig { allow_images: vec![bad.into()], ..DockerConfig::default() }.validate().is_err(), "{bad}");
        }
        assert!(DockerConfig { max_memory_mb: 8, ..DockerConfig::default() }.validate().is_err());
        assert!(DockerConfig { max_cpus: 500.0, ..DockerConfig::default() }.validate().is_err());
        assert!(serde_json::from_str::<DockerConfig>(r#"{"privileged": true}"#).is_err());
    }

    #[test]
    fn docker_errors_map_to_specific_codes() {
        let cases = [
            ("Cannot connect to the Docker daemon at npipe:////./pipe/docker_engine. Is the docker daemon running?", ErrorCode::Unavailable),
            ("error during connect: Get \"http://%2F%2F.%2Fpipe%2Fdocker_engine/v1.45/containers/json\"", ErrorCode::Unavailable),
            ("Error response from daemon: No such container: web", ErrorCode::NotFound),
            ("Error: No such image: x", ErrorCode::NotFound),
            ("pull access denied for x, repository does not exist", ErrorCode::NotFound),
            ("Error response from daemon: Conflict. The container name \"/web\" is already in use", ErrorCode::AlreadyExists),
            ("Bind for 127.0.0.1:8080 failed: port is already allocated", ErrorCode::Conflict),
            ("Got permission denied while trying to connect", ErrorCode::OsPermissionDenied),
            ("something odd", ErrorCode::BackendError),
        ];
        for (text, want) in cases {
            assert_eq!(docker_failure(text, "").code, want, "{text}");
        }
        assert!(docker_failure("Cannot connect to the Docker daemon", "").message.contains("Start Docker Desktop"));
    }

    #[test]
    fn inspect_summaries_keep_variable_names_and_drop_values() {
        let v = json!({"Id": "sha256:abcdef0123456789", "Name": "/web", "Config": {"Image": "nginx:1", "Env": ["DB_PASSWORD=hunter2", "PATH=/bin"], "Labels": {"aqua.managed": "true"}}, "State": {"Status": "running"}, "HostConfig": {"Privileged": false, "NetworkMode": "bridge"}, "Mounts": [{"Type": "bind", "Source": "/host", "Destination": "/data", "RW": false}]});
        let s = summarise_inspect(&v);
        assert_eq!(s["env_names"], json!(["DB_PASSWORD", "PATH"]));
        assert!(!s.to_string().contains("hunter2"));
        assert_eq!(s["name"], "web");
        assert_eq!(s["aqua_managed"], true);
        assert_eq!(s["mounts"][0]["read_write"], false);
    }
}
