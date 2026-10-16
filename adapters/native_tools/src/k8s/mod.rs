//! Kubernetes tools (`k8s.*`) built on `kubectl`.
//!
//! The model names a configured context and one of its allowed namespaces; it can never supply a
//! kubeconfig, a server address, a user to impersonate or an arbitrary resource kind. Secrets are
//! never readable (they are not in the kind list), manifests that ask for cluster-wide power or
//! host access are refused, and writes are off by default for anything that is not a `dev`
//! context. There is no `exec`, `cp`, `port-forward`, `proxy` or `edit`.

use crate::cli::{first_line, s, Cli, Ran};
use crate::fs::guard::{display, Op, PathGuard};
use aqua_tool_host::{
    Annotations, Availability, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Env {
    Dev,
    Staging,
    Prod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct K8sContext {
    /// The context name in your kubeconfig.
    pub name: String,
    /// Namespaces the model may use in this context. The first is the default.
    pub namespaces: Vec<String>,
    #[serde(default = "default_env")]
    pub env: Env,
    /// Allow apply, scale, restart and delete. Off unless set, and always asks when the context is not `dev`.
    #[serde(default)]
    pub write: bool,
}

fn default_env() -> Env {
    Env::Dev
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct K8sConfig {
    #[serde(default)]
    pub kubectl_path: Option<String>,
    /// Path to the kubeconfig to use; kubectl's default when absent.
    #[serde(default)]
    pub kubeconfig: Option<String>,
    #[serde(default)]
    pub contexts: Vec<K8sContext>,
    #[serde(default = "default_max_replicas")]
    pub max_replicas: u32,
}

fn default_max_replicas() -> u32 {
    20
}

impl Default for K8sConfig {
    fn default() -> Self {
        Self { kubectl_path: None, kubeconfig: None, contexts: Vec::new(), max_replicas: 20 }
    }
}

fn dns_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit()) && s.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit()) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
}

impl K8sConfig {
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for c in &self.contexts {
            if c.name.is_empty() || c.name.len() > 200 || c.name.starts_with('-') || !c.name.chars().all(|x| x.is_ascii_alphanumeric() || matches!(x, '.' | '_' | '-' | '@' | ':' | '/')) {
                return Err(format!("context '{}' is not a valid context name", c.name));
            }
            if !seen.insert(c.name.clone()) {
                return Err(format!("context '{}' is listed twice", c.name));
            }
            if c.namespaces.is_empty() || c.namespaces.iter().any(|n| !dns_name(n)) {
                return Err(format!("context '{}' needs a list of valid namespaces (never '*' or all namespaces)", c.name));
            }
        }
        if self.max_replicas == 0 || self.max_replicas > 500 {
            return Err("max_replicas must be 1-500".into());
        }
        Ok(())
    }
}

/// Kinds that can be read. Secrets are deliberately absent.
const READ_KINDS: &[&str] = &[
    "pods", "deployments", "replicasets", "statefulsets", "daemonsets", "services", "ingresses", "configmaps", "jobs", "cronjobs", "events", "endpoints", "persistentvolumeclaims",
    "horizontalpodautoscalers", "serviceaccounts", "resourcequotas", "limitranges", "networkpolicies", "nodes", "namespaces",
];
const CLUSTER_KINDS: &[&str] = &["nodes", "namespaces"];
const DELETE_KINDS: &[&str] = &["pods", "deployments", "services", "configmaps", "jobs", "cronjobs", "ingresses", "statefulsets", "daemonsets", "horizontalpodautoscalers"];

fn normalise_kind(k: &str) -> String {
    let k = k.to_ascii_lowercase();
    match k.as_str() {
        "po" | "pod" => "pods",
        "deploy" | "deployment" => "deployments",
        "rs" | "replicaset" => "replicasets",
        "sts" | "statefulset" => "statefulsets",
        "ds" | "daemonset" => "daemonsets",
        "svc" | "service" => "services",
        "ing" | "ingress" => "ingresses",
        "cm" | "configmap" => "configmaps",
        "job" => "jobs",
        "cj" | "cronjob" => "cronjobs",
        "ev" | "event" => "events",
        "ep" | "endpoint" => "endpoints",
        "pvc" | "persistentvolumeclaim" => "persistentvolumeclaims",
        "hpa" | "horizontalpodautoscaler" => "horizontalpodautoscalers",
        "sa" | "serviceaccount" => "serviceaccounts",
        "no" | "node" => "nodes",
        "ns" | "namespace" => "namespaces",
        other => return other.to_string(),
    }
    .to_string()
}

/// Kinds a manifest may not contain: cluster-wide power, secrets, and things that change how the cluster admits workloads.
const FORBIDDEN_MANIFEST_KINDS: &[&str] = &[
    "secret", "clusterrole", "clusterrolebinding", "role", "rolebinding", "customresourcedefinition", "mutatingwebhookconfiguration", "validatingwebhookconfiguration", "podsecuritypolicy",
    "namespace", "persistentvolume", "storageclass", "apiservice", "priorityclass", "runtimeclass", "csidriver", "certificatesigningrequest", "tokenreview",
];

/// Problems found in a manifest, or an empty list. `namespace` is the one the call is allowed to use.
pub fn scan_manifest(text: &str, namespace: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut docs = 1;
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        if raw.trim_end() == "---" {
            docs += 1;
            continue;
        }
        let line = raw.split('#').next().unwrap_or("").trim().to_ascii_lowercase();
        let compact = line.replace(' ', "").replace('"', "").replace('\'', "");
        let compact = compact.trim_start_matches('-').to_string();
        if let Some(kind) = compact.strip_prefix("kind:").or_else(|| compact.strip_prefix("\"kind\":")) {
            let k = kind.trim_matches(',');
            if FORBIDDEN_MANIFEST_KINDS.contains(&k) {
                problems.push(format!("line {n}: a manifest may not create a {k}"));
            }
        }
        if let Some(ns) = compact.strip_prefix("namespace:") {
            if ns != namespace {
                problems.push(format!("line {n}: the manifest names namespace '{ns}', but this call is limited to '{namespace}'"));
            }
        }
        for (pat, why) in [
            ("privileged:true", "privileged containers are not allowed"),
            ("allowprivilegeescalation:true", "privilege escalation is not allowed"),
            ("hostnetwork:true", "host networking is not allowed"),
            ("hostpid:true", "host PID namespace is not allowed"),
            ("hostipc:true", "host IPC namespace is not allowed"),
            ("hostpath:", "hostPath volumes give access to the node's files"),
            ("runasuser:0", "running as root is not allowed"),
            ("docker.sock", "the container runtime socket must not be mounted"),
            ("hostport:", "host ports are not allowed"),
            ("add:", "adding Linux capabilities is not allowed"),
        ] {
            if compact.starts_with(pat) || compact.contains(&format!("{{{pat}")) {
                problems.push(format!("line {n}: {why}"));
            }
        }
    }
    if docs > 50 {
        problems.push(format!("the manifest has {docs} documents; the limit is 50"));
    }
    problems
}

struct Inner {
    cfg: K8sConfig,
    guard: Arc<PathGuard>,
    cli: Cli,
}

/// The Kubernetes tools. State is behind an `Arc` so blocking tasks can own it.
pub struct K8sProvider(Arc<Inner>);

impl K8sProvider {
    pub fn new(cfg: K8sConfig, guard: Arc<PathGuard>, scratch: &Path) -> Result<Self, String> {
        cfg.validate()?;
        std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
        let cli = Cli::find("kubectl", &cfg.kubectl_path, &["%ProgramFiles%/Kubernetes", "%USERPROFILE%/.local/bin", "%ProgramFiles%/Docker/Docker/resources/bin"], scratch);
        Ok(Self(Arc::new(Inner { cfg, guard, cli })))
    }

    pub fn installed(&self) -> bool {
        self.0.cli.installed()
    }
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, msg)
}

fn arg_str<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Delete,
}

fn specs() -> Vec<(&'static str, &'static str, Kind, Value, Vec<&'static str>)> {
    let ctx = json!({"type": "string", "maxLength": 200});
    let ns = json!({"type": "string", "maxLength": 63});
    let kind = json!({"type": "string", "minLength": 1, "maxLength": 40});
    let name = json!({"type": "string", "minLength": 1, "maxLength": 253});
    let tail = json!({"type": "integer", "minimum": 1, "maximum": 2000, "default": 200});
    vec![
        ("k8s.contexts", "List the configured clusters, their allowed namespaces and whether writes are enabled.", Kind::Read, json!({}), vec![]),
        ("k8s.get", "List resources of a kind (pods, deployments, services…) or show one, in a namespace. Secrets are not readable.", Kind::Read, json!({"context": ctx.clone(), "namespace": ns.clone(), "kind": kind.clone(), "name": name.clone(), "selector": {"type": "string", "maxLength": 200}}), vec!["kind"]),
        ("k8s.describe", "Describe one resource: its state, events and settings. Environment values are hidden.", Kind::Read, json!({"context": ctx.clone(), "namespace": ns.clone(), "kind": kind.clone(), "name": name.clone()}), vec!["kind", "name"]),
        ("k8s.logs", "Read the last lines of a pod's log.", Kind::Read, json!({"context": ctx.clone(), "namespace": ns.clone(), "pod": name.clone(), "container": {"type": "string", "maxLength": 63}, "tail": tail, "since": {"type": "string", "maxLength": 20}, "previous": {"type": "boolean", "default": false}}), vec!["pod"]),
        ("k8s.events", "List recent events in a namespace, newest last.", Kind::Read, json!({"context": ctx.clone(), "namespace": ns.clone()}), vec![]),
        ("k8s.top", "Show CPU and memory use of pods or nodes (needs metrics-server).", Kind::Read, json!({"context": ctx.clone(), "namespace": ns.clone(), "kind": {"type": "string", "enum": ["pods", "nodes"], "default": "pods"}}), vec![]),
        ("k8s.rollout_status", "Wait up to a minute for a deployment, statefulset or daemonset rollout and report its status.", Kind::Read, json!({"context": ctx.clone(), "namespace": ns.clone(), "kind": {"type": "string", "enum": ["deployment", "statefulset", "daemonset"], "default": "deployment"}, "name": name.clone()}), vec!["name"]),
        ("k8s.apply", "Apply a manifest file from an allowed folder. A server-side dry run is shown first. Refuses cluster-wide, host-access and secret manifests. Asks.", Kind::Write, json!({"context": ctx.clone(), "namespace": ns.clone(), "file": {"type": "string", "minLength": 1, "maxLength": 1024}}), vec!["file"]),
        ("k8s.scale", "Set the replica count of a deployment or statefulset. Asks.", Kind::Write, json!({"context": ctx.clone(), "namespace": ns.clone(), "kind": {"type": "string", "enum": ["deployment", "statefulset"], "default": "deployment"}, "name": name.clone(), "replicas": {"type": "integer", "minimum": 0, "maximum": 500}}), vec!["name", "replicas"]),
        ("k8s.rollout_restart", "Restart the pods of a deployment, statefulset or daemonset one by one. Asks.", Kind::Write, json!({"context": ctx.clone(), "namespace": ns.clone(), "kind": {"type": "string", "enum": ["deployment", "statefulset", "daemonset"], "default": "deployment"}, "name": name.clone()}), vec!["name"]),
        ("k8s.delete", "Delete one resource by kind and name (never a namespace, node, volume or secret). Always asks.", Kind::Delete, json!({"context": ctx, "namespace": ns, "kind": kind, "name": name}), vec!["kind", "name"]),
    ]
}

/// Everything a call resolves to before it may run.
struct Target<'a> {
    ctx: &'a K8sContext,
    namespace: String,
}

impl Inner {
    fn target(&self, a: &Value) -> Result<Target<'_>, ToolError> {
        let name = arg_str(a, "context");
        let ctx = if name.is_empty() {
            match self.cfg.contexts.as_slice() {
                [only] => only,
                [] => return Err(ToolError::new(ErrorCode::NotFound, "no clusters are configured; add them to data/tools/k8s.json")),
                many => return Err(bad(format!("say which cluster: {}", many.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")))),
            }
        } else {
            self.cfg.contexts.iter().find(|c| c.name == name).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("no configured cluster '{name}'. Configured: {}", self.cfg.contexts.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", "))))?
        };
        let ns = match arg_str(a, "namespace") {
            "" => ctx.namespaces[0].clone(),
            n => {
                if !ctx.namespaces.iter().any(|x| x == n) {
                    return Err(ToolError::new(ErrorCode::PermissionDenied, format!("namespace '{n}' is not allowed in '{}'. Allowed: {}", ctx.name, ctx.namespaces.join(", "))));
                }
                n.to_string()
            }
        };
        Ok(Target { ctx, namespace: ns })
    }

    fn base(&self, t: &Target, cluster_scoped: bool) -> Vec<String> {
        let mut v = s(&["--request-timeout=30s"]);
        if let Some(k) = &self.cfg.kubeconfig {
            v.push(format!("--kubeconfig={}", crate::fs::guard::expand_env(k)));
        }
        v.push(format!("--context={}", t.ctx.name));
        if !cluster_scoped {
            v.push(format!("--namespace={}", t.namespace));
        }
        v
    }

    fn kubectl(&self, args: &[String], timeout_s: u64, cap_kb: usize, cancel: &AtomicBool) -> Result<Ran, ToolError> {
        // KUBECONFIG and friends are not inherited; the config file names the kubeconfig explicitly.
        self.cli.run(args, &[("KUBECACHEDIR", &std::env::temp_dir().to_string_lossy())], None, timeout_s, cap_kb, cancel)
    }

    fn ok(&self, args: &[String], timeout_s: u64, cap_kb: usize, cancel: &AtomicBool) -> Result<Ran, ToolError> {
        let r = self.kubectl(args, timeout_s, cap_kb, cancel)?;
        if r.code == Some(0) {
            Ok(r)
        } else {
            Err(kubectl_failure(&r.stderr, &r.stdout))
        }
    }

    fn read_kind(&self, k: &str, for_delete: bool) -> Result<String, ToolError> {
        let kind = normalise_kind(k);
        let list = if for_delete { DELETE_KINDS } else { READ_KINDS };
        if kind.contains("secret") {
            return Err(ToolError::new(ErrorCode::SecretBlocked, "secrets are never read or changed through these tools"));
        }
        if !list.contains(&kind.as_str()) {
            return Err(bad(format!("kind '{k}' is not available here. Available: {}", list.join(", "))));
        }
        Ok(kind)
    }

    fn resource_name(n: &str) -> Result<&str, ToolError> {
        if dns_name(n) { Ok(n) } else { Err(bad(format!("'{n}' is not a valid resource name"))) }
    }

    fn manifest(&self, a: &Value, ns: &str) -> Result<(std::path::PathBuf, String), ToolError> {
        let r = self.guard.resolve(arg_str(a, "file"), Op::Read)?;
        let low = r.path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if !matches!(low.as_str(), "yaml" | "yml" | "json") {
            return Err(bad("the manifest must be a .yaml, .yml or .json file"));
        }
        let text = std::fs::read_to_string(&r.path).map_err(|e| ToolError::new(ErrorCode::NotFound, format!("{}: {e}", display(&r.path))))?;
        if text.len() > 512 * 1024 {
            return Err(ToolError::new(ErrorCode::ResourceLimit, "the manifest is larger than 512 KB"));
        }
        let problems = scan_manifest(&text, ns);
        if !problems.is_empty() {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("the manifest asks for things Aqua does not allow: {}", problems.join("; "))).with_details(json!({"problems": problems})));
        }
        Ok((r.path, text))
    }

    fn write_allowed(&self, t: &Target) -> Result<(), ToolError> {
        if t.ctx.write {
            Ok(())
        } else {
            Err(ToolError::new(ErrorCode::PermissionDenied, format!("writes are not enabled for '{}'. Set \"write\": true for it in data/tools/k8s.json to allow them (each will still ask).", t.ctx.name)))
        }
    }
}

impl ToolProvider for K8sProvider {
    fn id(&self) -> &str {
        "k8s"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        specs()
            .into_iter()
            .map(|(name, description, kind, props, required)| ToolManifest {
                name: name.into(),
                title: name.into(),
                description: description.into(),
                classes: match kind {
                    Kind::Read => vec![PermClass::Read],
                    Kind::Write => vec![PermClass::Write, PermClass::Remote],
                    Kind::Delete => vec![PermClass::Write, PermClass::Remote, PermClass::Destructive],
                },
                default_decision: Decision::Auto,
                input_schema: json!({"type": "object", "properties": props, "required": required}),
                timeout_default_ms: 120_000,
                timeout_max_ms: 180_000,
                annotations: Annotations { read_only: kind == Kind::Read, destructive: kind == Kind::Delete, idempotent: kind == Kind::Read, open_world: true },
                untrusted_output: true,
            })
            .collect()
    }

    fn availability(&self, _tool: &str) -> Availability {
        if !self.0.cli.installed() {
            return Availability::Unavailable(self.0.cli.not_installed());
        }
        Availability::Available
    }

    fn classify(&self, tool: &str, a: &Value) -> Result<Classification, ToolError> {
        let s = &self.0;
        if tool == "k8s.contexts" {
            return Ok(Classification { classes: vec![PermClass::Read], target: "k8s.json".into(), summary: "List configured clusters".into(), target_allowed: true });
        }
        let (_, _, kind, _, _) = specs().into_iter().find(|x| x.0 == tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        let t = s.target(a)?;
        let label = format!("{} / {}", t.ctx.name, t.namespace);
        match tool {
            "k8s.get" | "k8s.describe" => {
                let k = s.read_kind(arg_str(a, "kind"), false)?;
                if !arg_str(a, "name").is_empty() {
                    Inner::resource_name(arg_str(a, "name"))?;
                }
                check_selector(arg_str(a, "selector"))?;
                return Ok(Classification { classes: vec![PermClass::Read], target: label.clone(), summary: format!("{tool} {k} in {label}"), target_allowed: true });
            }
            "k8s.logs" => {
                Inner::resource_name(arg_str(a, "pod"))?;
                for (k, max) in [("container", 63usize), ("since", 20)] {
                    let v = arg_str(a, k);
                    if !v.is_empty() && (v.len() > max || v.starts_with('-') || !v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))) {
                        return Err(bad(format!("{k} is not valid")));
                    }
                }
            }
            "k8s.rollout_status" | "k8s.rollout_restart" | "k8s.scale" => {
                Inner::resource_name(arg_str(a, "name"))?;
            }
            "k8s.events" | "k8s.top" => {}
            _ => {}
        }
        if kind == Kind::Read {
            return Ok(Classification { classes: vec![PermClass::Read], target: label.clone(), summary: format!("{tool} in {label}"), target_allowed: true });
        }
        // Everything below changes the cluster.
        s.write_allowed(&t)?;
        let mut classes = vec![PermClass::Write, PermClass::Remote];
        if t.ctx.env != Env::Dev {
            classes.push(PermClass::ExternalEffect);
        }
        let summary = match tool {
            "k8s.apply" => {
                let (file, _) = s.manifest(a, &t.namespace)?;
                // A server-side dry run: nothing is stored, and the user sees what would change.
                let cancel = AtomicBool::new(false);
                let mut v = s.base(&t, false);
                v.extend(super::cli::s(&["apply", "--dry-run=server", "-f", &file.to_string_lossy()]));
                let r = s.ok(&v, 60, 32, &cancel)?;
                let changes: Vec<&str> = r.stdout.lines().filter(|l| !l.trim().is_empty()).take(15).collect();
                format!("Apply {} to {label}. Dry run: {}", display(&file), changes.join("; "))
            }
            "k8s.scale" => {
                let n = a["replicas"].as_u64().unwrap_or(0) as u32;
                if n > s.cfg.max_replicas {
                    return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{n} replicas is above the limit of {} for this setup", s.cfg.max_replicas)));
                }
                format!("Scale {} {} to {n} replica(s) in {label}", arg_str(a, "kind").is_empty().then_some("deployment").unwrap_or(arg_str(a, "kind")), arg_str(a, "name"))
            }
            "k8s.rollout_restart" => format!("Restart {} {} in {label}", if arg_str(a, "kind").is_empty() { "deployment" } else { arg_str(a, "kind") }, arg_str(a, "name")),
            _ => {
                let k = s.read_kind(arg_str(a, "kind"), true)?;
                classes.push(PermClass::Destructive);
                Inner::resource_name(arg_str(a, "name"))?;
                format!("DELETE {k} {} in {label}", arg_str(a, "name"))
            }
        };
        Ok(Classification { classes, target: label, summary, target_allowed: false })
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
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("kubectl task failed: {e}")))?.map(ProviderOutput::marked_private)
        })
    }
}

fn check_selector(sel: &str) -> Result<(), ToolError> {
    if sel.is_empty() {
        return Ok(());
    }
    if sel.len() > 200 || sel.starts_with('-') || !sel.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '=' | '!' | ',' | '-')) {
        return Err(bad("the label selector may only use letters, digits and . _ / = ! , -"));
    }
    Ok(())
}

/// Hide the values in an `Environment:` block of `kubectl describe pod`: they routinely hold secrets.
pub fn hide_environment_values(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_env = false;
    let mut env_indent = 0usize;
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        if line.trim_end().ends_with("Environment:") {
            in_env = true;
            env_indent = indent;
            out.push(line.to_string());
            continue;
        }
        if in_env {
            if !line.trim().is_empty() && indent <= env_indent {
                in_env = false;
            } else if let Some((name, _)) = line.split_once(':') {
                out.push(format!("{name}:  <hidden>"));
                continue;
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

fn kubectl_failure(stderr: &str, stdout: &str) -> ToolError {
    let all = format!("{stderr}\n{stdout}");
    let low = all.to_ascii_lowercase();
    let first = first_line(&all);
    let has = |m: &str| low.contains(m);
    let code = if has("unable to connect to the server") || has("connection refused") || has("dial tcp") || has("no such host") || has("i/o timeout") && !has("timed out waiting") {
        ErrorCode::Unavailable
    } else if has("forbidden") {
        ErrorCode::OsPermissionDenied
    } else if has("unauthorized") || has("you must be logged in") || has("token has expired") {
        ErrorCode::AuthRequired
    } else if has("notfound") || has("not found") || has("does not exist") || has("no resources found") && false {
        ErrorCode::NotFound
    } else if has("already exists") {
        ErrorCode::AlreadyExists
    } else if has("timed out") || has("timeout") {
        ErrorCode::Timeout
    } else if has("invalid") || has("error validating") || has("unknown field") || has("is invalid") {
        ErrorCode::InvalidArguments
    } else {
        ErrorCode::BackendError
    };
    ToolError::new(code, first)
}

impl Inner {
    fn dispatch(&self, tool: &str, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        if tool == "k8s.contexts" {
            let rows: Vec<Value> = self.cfg.contexts.iter().map(|c| json!({"context": c.name, "namespaces": c.namespaces, "env": c.env, "write": c.write})).collect();
            let text = rows.iter().map(|r| format!("{} namespaces {} ({:?}, writes {})", r["context"].as_str().unwrap_or(""), r["namespaces"], r["env"], if r["write"] == true { "enabled" } else { "off" })).collect::<Vec<_>>().join("\n");
            return Ok(ProviderOutput { data: json!({ "contexts": rows }), text: if text.is_empty() { "No clusters are configured. Add them to data/tools/k8s.json.".into() } else { text }, ..Default::default() });
        }
        let t = self.target(a)?;
        let text_out = |r: Ran, data: Value| -> Result<ProviderOutput, ToolError> {
            let mut text = format!("{}{}", r.stdout, if r.stderr.trim().is_empty() { String::new() } else { format!("\n{}", r.stderr.trim()) }).trim().to_string();
            if r.truncated {
                text.push_str("\n… output cut");
            }
            Ok(ProviderOutput { data, text: if text.is_empty() { "Done.".into() } else { text }, ..Default::default() })
        };
        match tool {
            "k8s.get" | "k8s.describe" => {
                let kind = self.read_kind(arg_str(a, "kind"), false)?;
                let cluster = CLUSTER_KINDS.contains(&kind.as_str());
                let mut v = self.base(&t, cluster);
                v.push(if tool == "k8s.get" { "get" } else { "describe" }.into());
                v.push(kind.clone());
                if !arg_str(a, "name").is_empty() {
                    v.push(Inner::resource_name(arg_str(a, "name"))?.into());
                }
                if !arg_str(a, "selector").is_empty() {
                    check_selector(arg_str(a, "selector"))?;
                    v.push(format!("--selector={}", arg_str(a, "selector")));
                }
                if tool == "k8s.get" {
                    v.extend(s(&["-o", "wide"]));
                }
                let mut r = self.ok(&v, 60, 128, cancel)?;
                if tool == "k8s.describe" {
                    r.stdout = hide_environment_values(&r.stdout);
                }
                text_out(r, json!({"context": t.ctx.name, "namespace": t.namespace, "kind": kind}))
            }
            "k8s.logs" => {
                let mut v = self.base(&t, false);
                v.extend(s(&["logs", Inner::resource_name(arg_str(a, "pod"))?, &format!("--tail={}", a.get("tail").and_then(|x| x.as_u64()).unwrap_or(200).clamp(1, 2000))]));
                for (k, flag) in [("container", "-c"), ("since", "--since")] {
                    if !arg_str(a, k).is_empty() {
                        v.push(format!("{flag}={}", arg_str(a, k)));
                    }
                }
                if a.get("previous").and_then(|x| x.as_bool()).unwrap_or(false) {
                    v.push("--previous".into());
                }
                text_out(self.ok(&v, 60, 128, cancel)?, json!({"pod": arg_str(a, "pod")}))
            }
            "k8s.events" => {
                let mut v = self.base(&t, false);
                v.extend(s(&["get", "events", "--sort-by=.lastTimestamp"]));
                let r = self.ok(&v, 60, 64, cancel)?;
                let lines: Vec<&str> = r.stdout.lines().collect();
                let start = lines.len().saturating_sub(60);
                text_out(Ran { stdout: lines[start..].join("\n"), ..r }, json!({"namespace": t.namespace}))
            }
            "k8s.top" => {
                let nodes = arg_str(a, "kind") == "nodes";
                let mut v = self.base(&t, nodes);
                v.extend(s(&["top", if nodes { "nodes" } else { "pods" }]));
                text_out(self.ok(&v, 60, 64, cancel)?, json!({}))
            }
            "k8s.rollout_status" => {
                let kind = match arg_str(a, "kind") { "" => "deployment", k => k };
                let mut v = self.base(&t, false);
                v.extend(s(&["rollout", "status", &format!("{kind}/{}", Inner::resource_name(arg_str(a, "name"))?), "--timeout=60s"]));
                text_out(self.ok(&v, 90, 32, cancel)?, json!({"name": arg_str(a, "name")}))
            }
            "k8s.apply" => {
                self.write_allowed(&t)?;
                let (file, _) = self.manifest(a, &t.namespace)?;
                let mut v = self.base(&t, false);
                v.extend(s(&["apply", "-f", &file.to_string_lossy()]));
                text_out(self.ok(&v, 120, 64, cancel)?, json!({"file": display(&file)}))
            }
            "k8s.scale" => {
                self.write_allowed(&t)?;
                let n = a["replicas"].as_u64().unwrap_or(0) as u32;
                if n > self.cfg.max_replicas {
                    return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{n} replicas is above the limit of {}", self.cfg.max_replicas)));
                }
                let kind = match arg_str(a, "kind") { "" => "deployment", k => k };
                let mut v = self.base(&t, false);
                v.extend(s(&["scale", &format!("{kind}/{}", Inner::resource_name(arg_str(a, "name"))?), &format!("--replicas={n}")]));
                text_out(self.ok(&v, 60, 16, cancel)?, json!({"replicas": n}))
            }
            "k8s.rollout_restart" => {
                self.write_allowed(&t)?;
                let kind = match arg_str(a, "kind") { "" => "deployment", k => k };
                let mut v = self.base(&t, false);
                v.extend(s(&["rollout", "restart", &format!("{kind}/{}", Inner::resource_name(arg_str(a, "name"))?)]));
                text_out(self.ok(&v, 60, 16, cancel)?, json!({}))
            }
            "k8s.delete" => {
                self.write_allowed(&t)?;
                let kind = self.read_kind(arg_str(a, "kind"), true)?;
                let mut v = self.base(&t, false);
                v.extend(s(&["delete", &kind, Inner::resource_name(arg_str(a, "name"))?, "--wait=false"]));
                text_out(self.ok(&v, 60, 16, cancel)?, json!({"deleted": arg_str(a, "name")}))
            }
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_that_grant_power_or_reach_the_host_are_found() {
        let bad = [
            "kind: ClusterRoleBinding\nmetadata:\n  name: x",
            "kind: Secret\ndata:\n  a: b",
            "kind: \"Namespace\"",
            "kind: CustomResourceDefinition",
            "spec:\n  containers:\n  - securityContext:\n      privileged: true",
            "spec:\n  hostNetwork: true",
            "spec:\n  volumes:\n  - hostPath:\n      path: /",
            "securityContext:\n  runAsUser: 0",
            "metadata:\n  namespace: kube-system",
            "volumes:\n- name: s\n  hostPath: {path: /var/run/docker.sock}",
            "securityContext:\n  capabilities:\n    add: [\"SYS_ADMIN\"]",
        ];
        for text in bad {
            assert!(!scan_manifest(text, "app").is_empty(), "not caught:\n{text}");
        }
        let good = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  namespace: app\nspec:\n  replicas: 2\n  template:\n    spec:\n      containers:\n      - name: web\n        image: nginx:1.25 # privileged: true is only a comment\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: web\n";
        assert!(scan_manifest(good, "app").is_empty(), "{:?}", scan_manifest(good, "app"));
        assert!(!scan_manifest(&"---\n".repeat(60), "app").is_empty(), "too many documents");
    }

    #[test]
    fn config_validation_requires_specific_namespaces_and_valid_names() {
        let ctx = |n: &str, ns: Vec<&str>| K8sContext { name: n.into(), namespaces: ns.into_iter().map(String::from).collect(), env: Env::Dev, write: false };
        assert!(K8sConfig { contexts: vec![ctx("minikube", vec!["default", "app-1"])], ..Default::default() }.validate().is_ok());
        for bad in [ctx("", vec!["a"]), ctx("-x", vec!["a"]), ctx("a b", vec!["a"]), ctx("ok", vec![]), ctx("ok", vec!["*"]), ctx("ok", vec!["Bad_NS"]), ctx("ok", vec!["--all-namespaces"])] {
            assert!(K8sConfig { contexts: vec![bad.clone()], ..Default::default() }.validate().is_err(), "{bad:?}");
        }
        assert!(K8sConfig { contexts: vec![ctx("a", vec!["x"]), ctx("a", vec!["y"])], ..Default::default() }.validate().is_err());
        assert!(serde_json::from_str::<K8sConfig>(r#"{"contexts": [], "impersonate": "admin"}"#).is_err());
    }

    #[test]
    fn kinds_are_normalised_and_secrets_are_never_available() {
        assert_eq!(normalise_kind("po"), "pods");
        assert_eq!(normalise_kind("Deploy"), "deployments");
        assert_eq!(normalise_kind("svc"), "services");
        assert!(READ_KINDS.iter().all(|k| !k.contains("secret")));
        assert!(DELETE_KINDS.iter().all(|k| !CLUSTER_KINDS.contains(k) && !k.contains("secret") && *k != "persistentvolumeclaims"));
    }

    #[test]
    fn environment_values_in_describe_output_are_hidden() {
        let text = "Name: web\nContainers:\n  app:\n    Image: nginx\n    Environment:\n      DB_PASSWORD:  hunter2\n      MODE:         prod\n    Mounts:\n      /data from vol (rw)\nEvents:  <none>";
        let out = hide_environment_values(text);
        assert!(!out.contains("hunter2") && !out.contains("prod\n") && out.contains("DB_PASSWORD:  <hidden>") && out.contains("MODE:  <hidden>"), "{out}");
        assert!(out.contains("Image: nginx") && out.contains("/data from vol (rw)"), "{out}");
    }

    #[test]
    fn kubectl_errors_map_to_specific_codes() {
        for (text, want) in [
            ("Unable to connect to the server: dial tcp 10.0.0.1:6443: i/o timeout", ErrorCode::Unavailable),
            ("Error from server (Forbidden): pods is forbidden: User \"x\" cannot list", ErrorCode::OsPermissionDenied),
            ("error: You must be logged in to the server (Unauthorized)", ErrorCode::AuthRequired),
            ("Error from server (NotFound): deployments.apps \"x\" not found", ErrorCode::NotFound),
            ("Error from server (AlreadyExists): configmaps \"x\" already exists", ErrorCode::AlreadyExists),
            ("error: context \"nope\" does not exist", ErrorCode::NotFound),
            ("error: unknown field \"specs\"", ErrorCode::InvalidArguments),
            ("something odd", ErrorCode::BackendError),
        ] {
            assert_eq!(kubectl_failure(text, "").code, want, "{text}");
        }
    }

    #[test]
    fn selectors_are_checked() {
        for ok in ["", "app=web", "app=web,tier!=db", "env=prod"] {
            assert!(check_selector(ok).is_ok(), "{ok}");
        }
        for bad in ["--all-namespaces", "-l x", "a b", "a;b", "$(x)"] {
            assert!(check_selector(bad).is_err(), "{bad}");
        }
    }
}
