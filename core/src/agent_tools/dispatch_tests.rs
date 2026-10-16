//! Tests of `dispatch.rs`: the call of a tool through the host, one step of a command and what a file tool changed (move only: they were part of the inline module `tests` of `agent_tools.rs`; the path of every test is now `agent_tools::dispatch_tests::<name>`).

use super::tests::host;
use super::*;
use aqua_tool_host::*;
use serde_json::json;

/// Card OA11-02: a step of a command is a call of the host, and it says whether it ran.
#[test]
fn a_step_is_a_call_of_the_host_that_says_what_it_printed_or_why_it_did_not_run() {
    let h = host();
    let ok = call_step(&h, "fs.read_file", json!({"path": "a"}), Caller::Ui, "run_step_1").unwrap();
    assert!(ok.contains("ran fs.read_file"), "{ok}");
    let masked = call_step(&h, "fs.read_file", json!({"path": "a"}), Caller::Ui, "run_step_1").unwrap();
    assert!(!masked.contains("hunter2secret99"), "the host masks what a step printed: {masked}");
    let no = call_step(&h, "fs.nothing", json!({}), Caller::Ui, "run_step_1").unwrap_err();
    assert!(no.starts_with("ERROR [NOT_FOUND]"), "{no}");
    let restricted = call_step(&h, "fs.read_file", json!({}), Caller::Ui, "run_step_1").unwrap_err();
    assert!(restricted.starts_with("ERROR [INVALID_ARGUMENTS]"), "a missing path is refused by the host's own checks: {restricted}");
}

/// A pretend `fs.write`, `fs.edit` and `fs.delete`: the first two answer as the real ones do (`data` names the path and the bytes), the third always fails. Their classes are
/// those of a read, so that the standard mode lets them run unasked: what is tested is what the dispatch does with the answer, not the policy.
struct FileTools;
impl ToolProvider for FileTools {
    fn id(&self) -> &str {
        "filetools"
    }
    fn manifests(&self) -> Vec<ToolManifest> {
        ["fs.write", "fs.edit", "fs.delete"]
            .iter()
            .map(|name| ToolManifest {
                name: (*name).into(),
                title: (*name).into(),
                description: format!("Pretends to be {name}."),
                classes: vec![PermClass::Read],
                default_decision: Decision::Auto,
                input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}, "new": {"type": "string"}}}),
                timeout_default_ms: 5_000,
                timeout_max_ms: 5_000,
                annotations: Annotations { read_only: false, destructive: false, idempotent: false, open_world: false },
                untrusted_output: false,
            })
            .collect()
    }
    fn classify(&self, tool: &str, _a: &Value) -> Result<Classification, ToolError> {
        Ok(Classification { classes: vec![PermClass::Read], target: "x".into(), summary: tool.into(), target_allowed: true })
    }
    fn call<'a>(&'a self, tool: &'a str, a: Value, _c: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let result = match tool {
            "fs.delete" => Err(ToolError::new(ErrorCode::Unavailable, "the disk said no")),
            _ => {
                let path = a["path"].as_str().unwrap_or("").to_string();
                let bytes = a["content"].as_str().map_or(0, str::len);
                Ok(ProviderOutput { text: format!("changed {path}"), data: json!({"path": path, "bytes": bytes, "created": true, "replacements": 1}), ..Default::default() })
            }
        };
        Box::pin(async move { result })
    }
}

/// Card OA3-01 (AQ-04): this was a search of `call_with` for the call that tells the limits what a file tool changed. The dispatch is run with a pretend file tool, and what the
/// limits' counters hold for the run afterwards is what it told them.
#[test]
fn what_a_file_tool_changed_is_counted_for_the_run_that_called_it_and_a_call_that_failed_or_had_no_run_counts_for_nothing() {
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_files_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let h = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("audit_files.jsonl"))));
    h.register(Arc::new(FileTools)).unwrap();
    let counters = Arc::new(crate::autonomy::Autonomy::new(crate::autonomy::Settings::default()));
    crate::autonomy::with_autonomy(counters.clone(), || {
        let scope = RunScope::begin(None);
        let id = scope.id().to_string();
        let mine = |counters: &crate::autonomy::Autonomy| counters.running().into_iter().find(|r| r.run_id == id).map(|r| (r.files_changed, r.bytes_changed));
        assert_eq!(mine(&counters), None, "nothing was counted before a call");
        let said = call_with(&h, "fs.write", json!({"path": "E:/ws/a.txt", "content": "hello"}), Caller::Agent, &id, false, 0);
        assert!(said.contains("changed E:/ws/a.txt"), "{said}");
        assert_eq!(mine(&counters), Some((1, 5)), "a file written, five bytes, counted for the run");
        call_with(&h, "fs.edit", json!({"path": "E:/ws/b.txt", "new": "xy"}), Caller::Agent, &id, false, 0);
        assert_eq!(mine(&counters).map(|m| m.0), Some(2), "a second file");
        let failed = call_with(&h, "fs.delete", json!({"path": "E:/ws/c.txt"}), Caller::Agent, &id, false, 0);
        assert!(failed.starts_with("ERROR"), "{failed}");
        assert_eq!(mine(&counters).map(|m| m.0), Some(2), "a call that failed changed nothing");
        // A call with no run behind it (the app's own) is nobody's: it is not counted under an empty id.
        call_with(&h, "fs.write", json!({"path": "E:/ws/d.txt", "content": "x"}), Caller::Ui, "", false, 0);
        assert_eq!(mine(&counters).map(|m| m.0), Some(2));
        assert!(counters.running().iter().all(|r| !r.run_id.is_empty()), "nothing is counted under an empty id");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn calls_go_through_the_host_masked_and_failures_use_the_error_form() {
    let h = host();
    let ok = call(&h, "fs.read_file", json!({"path": "a"}), Caller::Agent, "r1");
    assert!(ok.contains("ran fs.read_file") && !ok.contains("hunter2secret99"), "{ok}");
    let bad = call(&h, "fs.read_file", json!({}), Caller::Agent, "r1");
    assert!(bad.starts_with("ERROR [INVALID_ARGUMENTS] fs.read_file"), "{bad}");
    let unknown = call(&h, "fs.nope", json!({}), Caller::Agent, "r1");
    assert!(unknown.starts_with("ERROR [NOT_FOUND]"), "{unknown}");
    let voice = call(&h, "fs.write_file", json!({"path": "a"}), Caller::Voice, "r2");
    assert!(voice.starts_with("ERROR"), "voice cannot change anything: {voice}");
    let deny = observation(&aqua_tool_host::ToolResult::failure("term.run", "r", ToolError::new(ErrorCode::ApprovalDenied, "Run cargo build"), 0));
    assert!(deny.starts_with("ERROR [CONFIRMATION_DENIED] term.run: The user did not approve: Run cargo build."), "{deny}");
    let outside = observation(&aqua_tool_host::ToolResult::failure("fs.read", "r", ToolError::new(ErrorCode::PermissionDenied, "'E:/workspace/hello.txt' is outside the folders Aqua may use. Use a path inside one of: @ws = C:/Users/x/AquaWorkspace"), 0));
    assert!(outside.starts_with("ERROR [INVALID_ARGUMENTS] fs.read:") && outside.contains("@ws = C:/Users/x/AquaWorkspace"), "a wrong path can be retried: {outside}");
    let protected = observation(&aqua_tool_host::ToolResult::failure("fs.read", "r", ToolError::new(ErrorCode::PermissionDenied, "'C:/a/.aqua_api_token' is protected"), 0));
    assert!(protected.starts_with("ERROR [PERMISSION_DENIED]"), "a protected path still stops the run: {protected}");
    let blocked = observation(&aqua_tool_host::ToolResult::failure("fs.read_file", "r", ToolError::new(ErrorCode::SecretBlocked, "holds secrets"), 0));
    assert!(blocked.starts_with("ERROR [PERMISSION_DENIED] fs.read_file: (SECRET_BLOCKED)"), "{blocked}");
    assert!(is_host_tool(Some(&h), "term.run") && !is_host_tool(Some(&h), "Click") && !is_host_tool(None, "term.run"));
    assert!(is_read_only(Some(&h), "fs.read_file") && !is_read_only(Some(&h), "fs.write_file"));
}

#[test]
fn recipes_never_keep_bodies_commands_or_secrets() {
    // `question` is what `research.run` is asked (card R2-10): a question is as private as a search query.
    let r = redact_args(&json!({"path": "C:/w/a.txt", "content": "top secret text", "command": "curl -H x", "api_key": "abc", "question": "what is my diagnosis", "limit": 5}));
    assert_eq!(r["path"], "C:/w/a.txt");
    assert_eq!(r["limit"], 5);
    for k in ["content", "command", "api_key", "question"] {
        assert!(r[k].as_str().unwrap().starts_with("<redacted"), "{k}: {r}");
    }
    assert!(!r.to_string().contains("top secret"));
}
