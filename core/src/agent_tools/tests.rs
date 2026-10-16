//! The pretend providers and the hosts built from them that the tests of `agent_tools` share (move only: they were part of the inline module `tests` of `agent_tools.rs`; `crate::agent_tools::tests::wide_host` is where it was).

use super::*;
use aqua_tool_host::*;
use serde_json::json;

struct Fake;
impl ToolProvider for Fake {
    fn id(&self) -> &str {
        "fake"
    }
    fn manifests(&self) -> Vec<ToolManifest> {
        let m = |name: &str, ro: bool| ToolManifest {
            name: name.into(),
            title: name.into(),
            description: format!("Does {name}."),
            classes: if ro { vec![PermClass::Read] } else { vec![PermClass::Write] },
            default_decision: Decision::Auto,
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["path"]}),
            timeout_default_ms: 5_000,
            timeout_max_ms: 5_000,
            annotations: Annotations { read_only: ro, destructive: false, idempotent: ro, open_world: false },
            untrusted_output: ro,
        };
        let mut v = vec![m("fs.write_file", false), m("fs.read_file", true), m("git.status", true), m("term.run", false), m("web.fetch", true)];
        for i in 0..14 {
            v.push(m(&format!("fs.tool_{i:02}"), true));
        }
        v
    }
    fn classify(&self, tool: &str, _a: &Value) -> Result<Classification, ToolError> {
        let ro = tool.contains("read") || tool.contains("status") || tool.contains("fetch") || tool.contains("tool_");
        Ok(Classification { classes: if ro { vec![PermClass::Read] } else { vec![PermClass::Write] }, target: "x".into(), summary: tool.into(), target_allowed: true })
    }
    fn call<'a>(&'a self, tool: &'a str, _a: Value, _c: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let t = tool.to_string();
        Box::pin(async move { Ok(ProviderOutput { text: format!("ran {t} password=hunter2secret99"), ..Default::default() }) })
    }
}

pub(crate) fn host() -> ToolHost {
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let h = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("audit.jsonl"))));
    h.register(Arc::new(Fake)).unwrap();
    h
}

const REAL_READS: &[&str] = &["fs.list", "fs.find", "fs.grep", "fs.read", "fs.stat", "term.output", "term.sessions", "git.status", "git.diff", "git.log", "git.show", "git.blame", "git.branch"];
const REAL_WRITES: &[&str] = &[
    "fs.write", "fs.edit", "fs.patch", "fs.copy", "fs.move", "fs.delete", "fs.restore", "fs.set_attrs", "term.run", "term.start", "term.stop", "git.add", "git.commit", "git.push", "git.pull", "git.merge",
];

/// The real names of the file, terminal and git tools, as the host registers them.
struct RealNames;
impl ToolProvider for RealNames {
    fn id(&self) -> &str {
        "real-names"
    }
    fn manifests(&self) -> Vec<ToolManifest> {
        let m = |name: &str, ro: bool| ToolManifest {
            name: name.into(),
            title: name.into(),
            description: format!("Does {name}."),
            classes: if ro { vec![PermClass::Read] } else { vec![PermClass::Write] },
            default_decision: Decision::Auto,
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            timeout_default_ms: 5_000,
            timeout_max_ms: 5_000,
            annotations: Annotations { read_only: ro, destructive: false, idempotent: ro, open_world: false },
            untrusted_output: ro,
        };
        REAL_READS.iter().map(|n| m(n, true)).chain(REAL_WRITES.iter().map(|n| m(n, false))).collect()
    }
    fn classify(&self, tool: &str, _a: &Value) -> Result<Classification, ToolError> {
        let ro = REAL_READS.contains(&tool);
        Ok(Classification { classes: if ro { vec![PermClass::Read] } else { vec![PermClass::Write] }, target: "x".into(), summary: tool.into(), target_allowed: true })
    }
    fn call<'a>(&'a self, tool: &'a str, _a: Value, _c: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let t = tool.to_string();
        Box::pin(async move { Ok(ProviderOutput { text: format!("ran {t}"), ..Default::default() }) })
    }
}

pub(super) fn real_host() -> ToolHost {
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_real_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let h = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("audit.jsonl"))));
    h.register(Arc::new(RealNames)).unwrap();
    h
}

// ---- card OA5-01: every tool the prompt names is reachable --------------------------------------------------------------------------------

/// More of the real names, for what a coding goal is shown beyond the file, terminal and git tools: (name, only reads).
const WIDE_EXTRA: &[(&str, bool)] = &[
    ("term.shell", false),
    ("git.reset", false),
    ("ssh.exec", false),
    ("db.execute", false),
    ("db.query", true),
    ("db.schema", true),
    ("docker.compose_logs", true),
    ("docker.logs", true),
    ("docker.ps", true),
    ("docker.rm", false),
    ("forge.issues", true),
    ("forge.prs", true),
    ("forge.pr", true),
    ("forge.pr_create", false),
    ("k8s.get", true),
    ("k8s.logs", true),
    ("mon.log_tail", true),
    ("mon.health", true),
    ("web.search", true),
    ("web.read", true),
];

/// The names of `RealNames` and the tools of the packs a coding goal may add one of.
struct WideNames;
impl ToolProvider for WideNames {
    fn id(&self) -> &str {
        "wide-names"
    }
    fn manifests(&self) -> Vec<ToolManifest> {
        let m = |name: &str, ro: bool| ToolManifest {
            name: name.into(),
            title: name.into(),
            description: format!("Does {name}."),
            classes: if ro { vec![PermClass::Read] } else { vec![PermClass::Write] },
            default_decision: Decision::Auto,
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            timeout_default_ms: 5_000,
            timeout_max_ms: 5_000,
            annotations: Annotations { read_only: ro, destructive: false, idempotent: ro, open_world: false },
            untrusted_output: ro,
        };
        let mut v = RealNames.manifests();
        v.extend(WIDE_EXTRA.iter().map(|(n, ro)| m(n, *ro)));
        v
    }
    fn classify(&self, tool: &str, _a: &Value) -> Result<Classification, ToolError> {
        let ro = REAL_READS.contains(&tool) || WIDE_EXTRA.iter().any(|(n, ro)| *n == tool && *ro);
        Ok(Classification { classes: if ro { vec![PermClass::Read] } else { vec![PermClass::Write] }, target: "x".into(), summary: tool.into(), target_allowed: true })
    }
    fn call<'a>(&'a self, tool: &'a str, _a: Value, _c: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let t = tool.to_string();
        Box::pin(async move { Ok(ProviderOutput { text: format!("ran {t}"), ..Default::default() }) })
    }
}

pub(crate) fn wide_host() -> ToolHost {
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_wide_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let h = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("audit.jsonl"))));
    h.register(Arc::new(WideNames)).unwrap();
    h
}

pub(super) fn shown_names(h: &ToolHost, goal: &str) -> Vec<String> {
    tools_for_goal(h, goal).into_iter().map(|t| t.name).collect()
}
