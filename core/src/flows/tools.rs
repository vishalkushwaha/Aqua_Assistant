//! Flows as tools for the agent: `flows.list`, `flows.run` and `flows.status`. The agent can see which flows the person made, start one with its
//! inputs, and ask how a run is going. Starting one is **an effect on the outside world** (a flow can send, post, write, run programs), so
//! `flows.run` is classed as one and always asks, showing the flow's name and the inputs; a flow's own steps keep their own approvals as they
//! always did, and a run started this way is marked as started by the agent.
//!
//! What a flow returns is text its steps produced (pages, files, other programs'), so `flows.status` is untrusted output.

use crate::flows::model::{current_owner, Flow, VarKind};
use crate::flows::run::model::Run;
use aqua_tool_host::{
    Annotations, ChangePreview, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// How many flows `flows.list` names, and how much of an input value the question shows.
const MAX_LISTED: usize = 50;
const SHOWN_VALUE_CHARS: usize = 120;
/// How much of one input value the preview of a start shows (card OA6-04).
const PREVIEW_VALUE_CHARS: usize = 2000;

/// What the tools need of the flow engine (the live one, or a stand-in in the tests).
pub trait FlowsApi: Send + Sync {
    fn list(&self) -> Result<Vec<Flow>, String>;
    fn get(&self, id: i64) -> Result<Option<Flow>, String>;
    /// Start a run (it goes on in the background) and answer at once.
    fn start(&self, flow_id: i64, inputs: Map<String, Value>, dry_run: bool) -> Result<Run, String>;
    fn run(&self, run_id: i64) -> Result<Option<Run>, String>;
}

/// The engine of this program.
pub struct Live;

impl FlowsApi for Live {
    fn list(&self) -> Result<Vec<Flow>, String> {
        let store = crate::api_gateway::flow_api::store();
        let mut out = Vec::new();
        for s in store.list(current_owner()).map_err(|e| e.to_string())?.into_iter().take(MAX_LISTED) {
            if let Some(f) = store.get(current_owner(), s.id).map_err(|e| e.to_string())? {
                out.push(f);
            }
        }
        Ok(out)
    }

    fn get(&self, id: i64) -> Result<Option<Flow>, String> {
        crate::api_gateway::flow_api::store().get(current_owner(), id).map_err(|e| e.to_string())
    }

    fn start(&self, flow_id: i64, inputs: Map<String, Value>, dry_run: bool) -> Result<Run, String> {
        let runner = crate::flows::run::manager::runner().ok_or("the flow engine is not running")?;
        runner.start(current_owner(), flow_id, inputs, Value::Null, dry_run, "agent").map_err(|e| e.to_string())
    }

    fn run(&self, run_id: i64) -> Result<Option<Run>, String> {
        let runner = crate::flows::run::manager::runner().ok_or("the flow engine is not running")?;
        runner.store().get_run(current_owner(), run_id).map_err(|e| e.to_string())
    }
}

pub struct FlowsProvider(pub Arc<dyn FlowsApi>);

fn one_line(v: &Value, max: usize) -> String {
    let t = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    t.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

/// An input as the question shows it: on one line, and when it is cut, saying by how much, so the person knows there is more than they see.
fn shown(v: &Value) -> String {
    let all = one_line(v, usize::MAX);
    let n = all.chars().count();
    if n <= SHOWN_VALUE_CHARS {
        return all;
    }
    format!("{}… (+{} more characters)", all.chars().take(SHOWN_VALUE_CHARS).collect::<String>(), n - SHOWN_VALUE_CHARS)
}

fn kind_name(k: &VarKind) -> String {
    format!("{k:?}").to_ascii_lowercase()
}

fn arg_id(args: &Value, key: &str) -> Result<i64, ToolError> {
    args.get(key).and_then(|v| v.as_i64()).filter(|n| *n > 0).ok_or_else(|| ToolError::new(ErrorCode::InvalidArguments, format!("{key} is the number of a flow or a run (see flows.list)")))
}

/// The inputs a call gives, checked against what the flow declares: only its own variables, each a plain value.
fn checked_inputs(flow: &Flow, args: &Value) -> Result<Map<String, Value>, ToolError> {
    let given = match args.get("inputs") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(ToolError::new(ErrorCode::InvalidArguments, "inputs is an object: {\"name\": value}")),
    };
    for (k, v) in &given {
        if !flow.def.variables.iter().any(|d| &d.name == k) {
            let names = flow.def.variables.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ");
            return Err(ToolError::new(ErrorCode::InvalidArguments, format!("the flow \"{}\" has no input called {k:?}{}", flow.def.name, if names.is_empty() { " (it takes none)".to_string() } else { format!(" (its inputs: {names})") })));
        }
        if !matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
            return Err(ToolError::new(ErrorCode::InvalidArguments, format!("the input {k:?} is a text, a number or true/false")));
        }
    }
    Ok(given)
}

impl ToolProvider for FlowsProvider {
    fn id(&self) -> &str {
        "flows"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let m = |name: &str, description: &str, classes: Vec<PermClass>, schema: Value, read_only: bool| ToolManifest {
            name: name.into(),
            title: name.into(),
            description: description.into(),
            classes,
            default_decision: Decision::Auto,
            input_schema: schema,
            timeout_default_ms: 15_000,
            timeout_max_ms: 30_000,
            annotations: Annotations { read_only, destructive: false, idempotent: read_only, open_world: !read_only },
            untrusted_output: true,
        };
        vec![
            m("flows.list", "List the flows the user made: their numbers, names and the inputs each takes.", vec![PermClass::Read], json!({"type": "object", "properties": {}}), true),
            m(
                "flows.run",
                "Start one of the user's flows by its number, with its inputs. It runs in the background; flows.status says how it goes. Always asks.",
                vec![PermClass::ExternalEffect],
                json!({"type": "object", "properties": {"flow_id": {"type": "integer", "minimum": 1}, "inputs": {"type": "object", "additionalProperties": true}, "dry_run": {"type": "boolean", "default": false}}, "required": ["flow_id"]}),
                false,
            ),
            m("flows.status", "How a run of a flow is going: its state, what ended it, and how long it took.", vec![PermClass::Read], json!({"type": "object", "properties": {"run_id": {"type": "integer", "minimum": 1}}, "required": ["run_id"]}), true),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        match tool {
            "flows.run" => {
                let id = arg_id(args, "flow_id")?;
                let flow = self.0.get(id).map_err(|e| ToolError::new(ErrorCode::BackendError, e))?.ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("there is no flow number {id} (see flows.list)")))?;
                let inputs = checked_inputs(&flow, args)?;
                let shown = inputs.iter().map(|(k, v)| format!("{k}={}", shown(v))).collect::<Vec<_>>().join(", ");
                let dry = if args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false) { " (a dry run: nothing is sent or changed)" } else { "" };
                Ok(Classification {
                    classes: vec![],
                    target: format!("flow {id}"),
                    summary: format!("Run the flow \"{}\"{}{dry}", one_line(&json!(flow.def.name), 80), if shown.is_empty() { String::new() } else { format!(" with {shown}") }),
                    target_allowed: false,
                })
            }
            "flows.status" => Ok(Classification { classes: vec![], target: "flow runs".into(), summary: "Look at a flow run".into(), target_allowed: true }),
            _ => Ok(Classification { classes: vec![], target: "flows".into(), summary: "List the flows".into(), target_allowed: true }),
        }
    }

    /// The inputs a flow is started with, each one whole (card OA6-04, SE-03): the question's own sentence cuts a value at 120 characters, and what a value says is what the flow will
    /// send, post or write. A value is shown as text between quotes with its line breaks written out, so that it cannot pass for another input; a value longer than
    /// [`PREVIEW_VALUE_CHARS`] is cut and says how much is left out.
    fn approval_preview(&self, tool: &str, args: &Value) -> Option<ChangePreview> {
        if tool != "flows.run" {
            return None;
        }
        let flow = self.0.get(arg_id(args, "flow_id").ok()?).ok()??;
        let inputs = checked_inputs(&flow, args).ok()?;
        if inputs.is_empty() {
            return None;
        }
        let mut lines = vec![format!("Inputs of the flow \"{}\":", one_line(&json!(flow.def.name), 80))];
        for (name, value) in &inputs {
            let whole = match value {
                Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
                other => other.to_string(),
            };
            let n = whole.chars().count();
            lines.push(if n <= PREVIEW_VALUE_CHARS { format!("{name} = {whole}") } else { format!("{name} = {}\u{2026} ({} more characters not shown)", whole.chars().take(PREVIEW_VALUE_CHARS).collect::<String>(), n - PREVIEW_VALUE_CHARS) });
        }
        Some(ChangePreview::new(lines.join("\n")))
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let api = self.0.clone();
        let tool = tool.to_string();
        let caller_read_outside_text = ctx.run_holds_outside_text;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> Result<ProviderOutput, ToolError> {
                let backend = |e: String| ToolError::new(ErrorCode::BackendError, e);
                match tool.as_str() {
                    "flows.list" => {
                        let flows = api.list().map_err(backend)?;
                        let rows: Vec<Value> = flows
                            .iter()
                            .map(|f| json!({"id": f.id, "name": f.def.name, "description": one_line(&json!(f.def.description), 160), "inputs": f.def.variables.iter().map(|v| json!({"name": v.name, "kind": kind_name(&v.kind), "required": v.required, "choices": v.choices})).collect::<Vec<_>>()}))
                            .collect();
                        let text = if flows.is_empty() {
                            "The user has not made any flows.".to_string()
                        } else {
                            flows
                                .iter()
                                .map(|f| {
                                    let inputs = f.def.variables.iter().map(|v| format!("{}{}", v.name, if v.required { "" } else { "?" })).collect::<Vec<_>>().join(", ");
                                    format!("{}: {}{}", f.id, one_line(&json!(f.def.name), 80), if inputs.is_empty() { String::new() } else { format!(" (inputs: {inputs})") })
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        };
                        Ok(ProviderOutput { data: json!({"flows": rows}), text, ..Default::default() })
                    }
                    "flows.run" => {
                        let id = arg_id(&args, "flow_id")?;
                        let flow = api.get(id).map_err(backend)?.ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("there is no flow number {id}")))?;
                        let mut inputs = checked_inputs(&flow, &args)?;
                        // A run that had read text from outside chose these inputs after reading it: in the flow they are as much outside as a page is (card R6-11).
                        if caller_read_outside_text && !inputs.is_empty() {
                            let names: Vec<Value> = inputs.keys().map(|k| Value::String(k.clone())).collect();
                            inputs.insert(crate::flows::run::model::OUTSIDE_VARS.to_string(), Value::Array(names));
                        }
                        let dry = args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
                        let run = api.start(id, inputs, dry).map_err(|e| ToolError::new(ErrorCode::BackendError, one_line(&json!(e), 300)))?;
                        Ok(ProviderOutput {
                            data: json!({"run_id": run.id, "flow_id": id, "status": run.status, "dry_run": run.dry_run}),
                            text: format!("Started run {} of \"{}\"{}. It goes on in the background: flows.status run_id={} says how it is going.", run.id, one_line(&json!(flow.def.name), 80), if dry { " (a dry run)" } else { "" }, run.id),
                            ..Default::default()
                        })
                    }
                    _ => {
                        let id = arg_id(&args, "run_id")?;
                        let run = api.run(id).map_err(backend)?.ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("there is no run number {id}")))?;
                        let secs = if run.finished_at > 0 && run.started_at > 0 { Some(run.finished_at - run.started_at) } else { None };
                        let error = if run.error.is_empty() { String::new() } else { format!(" It stopped with: {}", one_line(&json!(run.error), 300)) };
                        Ok(ProviderOutput {
                            data: json!({"run_id": run.id, "flow_id": run.flow_id, "name": run.name, "status": run.status, "error": one_line(&json!(run.error), 300), "seconds": secs}),
                            text: format!("Run {} of \"{}\": {:?}.{error}{}", run.id, one_line(&json!(run.name), 80), run.status, secs.map(|s| format!(" It took {s} s.")).unwrap_or_default()),
                            ..Default::default()
                        })
                    }
                }
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the flows task stopped unexpectedly: {e}")))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flows::model::{FlowDef, VariableDef};
    use crate::flows::run::model::RunStatus;
    use aqua_tool_host::*;
    use std::sync::Mutex;

    fn flow(id: i64, name: &str, vars: &[(&str, bool)]) -> Flow {
        let def = FlowDef {
            name: name.into(),
            description: "does a thing\nover two lines".into(),
            variables: vars.iter().map(|(n, req)| VariableDef { name: n.to_string(), label: String::new(), kind: VarKind::default(), required: *req, default: None, choices: vec![] }).collect(),
            ..Default::default()
        };
        Flow { id, owner_id: "local".into(), version: 1, created_at: 0, updated_at: 0, def }
    }

    fn a_run(id: i64, flow_id: i64, name: &str, status: RunStatus) -> Run {
        Run {
            id,
            flow_id,
            owner_id: "local".into(),
            flow_version: 1,
            name: name.into(),
            status,
            trigger_kind: "agent".into(),
            dry_run: false,
            vars: Map::new(),
            payload: Value::Null,
            created_at: 0,
            started_at: 0,
            finished_at: 0,
            error: String::new(),
            from_run: None,
            def: FlowDef::default(),
        }
    }

    struct Fake {
        flows: Vec<Flow>,
        started: Mutex<Vec<(i64, Map<String, Value>, bool)>>,
    }

    impl FlowsApi for Fake {
        fn list(&self) -> Result<Vec<Flow>, String> {
            Ok(self.flows.clone())
        }
        fn get(&self, id: i64) -> Result<Option<Flow>, String> {
            Ok(self.flows.iter().find(|f| f.id == id).cloned())
        }
        fn start(&self, flow_id: i64, inputs: Map<String, Value>, dry_run: bool) -> Result<Run, String> {
            self.started.lock().unwrap().push((flow_id, inputs.clone(), dry_run));
            Ok(Run { dry_run, vars: inputs, ..a_run(77, flow_id, "x", RunStatus::Running) })
        }
        fn run(&self, run_id: i64) -> Result<Option<Run>, String> {
            Ok((run_id == 77).then(|| Run { error: "step 3 failed: \u{7}the page said NO".into(), started_at: 100, finished_at: 112, ..a_run(77, 1, "Report", RunStatus::Failed) }))
        }
    }

    struct Ask(Mutex<Vec<ApprovalRequest>>, ApprovalOutcome);

    impl Approver for Ask {
        fn request(&self, req: ApprovalRequest) -> ProviderFuture<'_, ApprovalOutcome> {
            self.0.lock().unwrap().push(req);
            let a = self.1;
            Box::pin(async move { a })
        }
    }

    fn rig(answer: ApprovalOutcome) -> (ToolHost, Arc<Fake>, Arc<Ask>) {
        let dir = std::env::temp_dir().join(format!("aqua_flow_tools_{}_{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')', ' '], "_"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let host = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Developer)), Arc::new(AuditLedger::open(dir.join("audit.jsonl"))));
        let api = Arc::new(Fake { flows: vec![flow(1, "Morning report", &[("city", true), ("count", false)]), flow(2, "Cleanup", &[])], started: Mutex::new(vec![]) });
        let ask = Arc::new(Ask(Mutex::new(vec![]), answer));
        host.set_approver(ask.clone());
        host.register(Arc::new(FlowsProvider(api.clone()))).unwrap();
        (host, api, ask)
    }

    fn call(host: &ToolHost, tool: &str, args: Value) -> ToolResult {
        let mut ctx = RequestContext::new(Caller::Agent);
        ctx.agent_run_id = "run1".into();
        host.call_blocking(ToolRequest { tool: tool.into(), arguments: args, context: ctx })
    }

    #[test]
    fn the_agent_can_list_the_flows_and_see_what_each_takes() {
        let (host, _, ask) = rig(ApprovalOutcome::Denied);
        let res = call(&host, "flows.list", json!({}));
        assert!(res.ok, "{}", res.text);
        assert!(res.text.contains("1: Morning report (inputs: city, count?)") && res.text.contains("2: Cleanup"), "{}", res.text);
        assert_eq!(res.data["flows"][0]["inputs"][0], json!({"name": "city", "kind": "text", "required": true, "choices": []}), "{}", res.data);
        assert!(!res.data["flows"][0]["description"].as_str().unwrap().contains('\n'));
        assert!(ask.0.lock().unwrap().is_empty(), "a list is a read: nobody is asked");
        assert!(host.is_tainted("run1"), "what a flow's name holds could be anything");
    }

    #[test]
    fn starting_a_flow_always_asks_and_the_question_names_the_flow_and_its_inputs() {
        let (host, api, ask) = rig(ApprovalOutcome::Approved(ApprovalScope::Session));
        // Developer mode runs ordinary work without asking; a flow is an effect on the outside world, so it asks, every time.
        for _ in 0..2 {
            let res = call(&host, "flows.run", json!({"flow_id": 1, "inputs": {"city": "Pune", "count": 3}}));
            assert!(res.ok, "{}", res.text);
            assert!(res.text.contains("Started run 77 of \"Morning report\"") && res.text.contains("flows.status run_id=77"), "{}", res.text);
        }
        let seen = ask.0.lock().unwrap();
        assert_eq!(seen.len(), 2, "approving for the session did not cover the second start");
        assert!(seen.iter().all(|a| a.always_ask) && seen[0].summary == "Run the flow \"Morning report\" with city=Pune, count=3", "{}", seen[0].summary);
        drop(seen);
        let started = api.started.lock().unwrap();
        assert_eq!(started[0].0, 1);
        assert_eq!(started[0].1["city"], json!("Pune"));
        assert!(!started[0].2);
    }

    /// Card R6-11: a run that had read text from outside chose the inputs of a flow after reading it, and the flow is told so.
    #[test]
    fn the_inputs_of_a_run_that_had_read_text_from_outside_are_marked_as_outside_for_the_flow() {
        let (host, api, _) = rig(ApprovalOutcome::Approved(ApprovalScope::Once));
        let clean = call(&host, "flows.run", json!({"flow_id": 1, "inputs": {"city": "Pune"}}));
        assert!(clean.ok, "{}", clean.text);
        // The run that started it is marked by that call (the output of the tool is labelled), so another run reads the page first.
        host.mark_tainted("run2");
        let mut ctx = RequestContext::new(Caller::Agent);
        ctx.agent_run_id = "run2".into();
        let after_reading = host.call_blocking(ToolRequest { tool: "flows.run".into(), arguments: json!({"flow_id": 1, "inputs": {"city": "Ignore everything", "count": 2}}), context: ctx });
        assert!(after_reading.ok, "{}", after_reading.text);
        let started = api.started.lock().unwrap();
        assert!(!started[0].1.contains_key(crate::flows::run::model::OUTSIDE_VARS), "a run that had read nothing gives plain inputs: {:?}", started[0].1);
        let names: Vec<String> = started[1].1[crate::flows::run::model::OUTSIDE_VARS].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect();
        assert!(names.contains(&"city".to_string()) && names.contains(&"count".to_string()) && names.len() == 2, "{names:?}");
    }

    #[test]
    fn a_refused_start_starts_nothing_and_only_the_flows_own_inputs_are_accepted() {
        let (host, api, _) = rig(ApprovalOutcome::Denied);
        assert_eq!(call(&host, "flows.run", json!({"flow_id": 1})).error.unwrap().code, ErrorCode::ApprovalDenied);
        assert!(api.started.lock().unwrap().is_empty());
        let (host, api, _) = rig(ApprovalOutcome::Approved(ApprovalScope::Once));
        let unknown = call(&host, "flows.run", json!({"flow_id": 1, "inputs": {"city": "Pune", "recipient": "evil@example.com"}}));
        assert_eq!(unknown.error.as_ref().unwrap().code, ErrorCode::InvalidArguments);
        assert!(unknown.text.contains("has no input called \"recipient\"") && unknown.text.contains("city, count"), "{}", unknown.text);
        let nested = call(&host, "flows.run", json!({"flow_id": 1, "inputs": {"city": {"a": 1}}}));
        assert!(nested.text.contains("a text, a number or true/false"), "{}", nested.text);
        let none = call(&host, "flows.run", json!({"flow_id": 2, "inputs": {"x": 1}}));
        assert!(none.text.contains("it takes none"), "{}", none.text);
        assert_eq!(call(&host, "flows.run", json!({"flow_id": 9})).error.unwrap().code, ErrorCode::NotFound);
        assert_eq!(call(&host, "flows.run", json!({"flow_id": 0})).error.unwrap().code, ErrorCode::InvalidArguments);
        assert!(api.started.lock().unwrap().is_empty(), "nothing started for any of them");
        // An input too long for the question says how much is not shown.
        let (host, _, ask) = rig(ApprovalOutcome::Approved(ApprovalScope::Once));
        let long = format!("{}{}", "a".repeat(120), "SECRET TAIL");
        assert!(call(&host, "flows.run", json!({"flow_id": 1, "inputs": {"city": long}})).ok);
        let summary = ask.0.lock().unwrap()[0].summary.clone();
        assert!(summary.contains("… (+11 more characters)") && !summary.contains("SECRET TAIL"), "{summary}");
        // A dry run says so in the question.
        let (host, _, ask) = rig(ApprovalOutcome::Approved(ApprovalScope::Once));
        assert!(call(&host, "flows.run", json!({"flow_id": 2, "dry_run": true})).ok);
        assert!(ask.0.lock().unwrap()[0].summary.contains("a dry run: nothing is sent or changed"));
    }

    /// Card OA6-04 (SE-03): the sentence of the question cuts an input at 120 characters, and what an input says is what the flow will send, post or write. The question carries each
    /// input whole (up to 2,000 characters), as text with its line breaks written out, so that one cannot pass for another.
    #[test]
    fn flows_run_question_carries_every_input_whole_and_says_when_it_cuts_one() {
        let (host, _, ask) = rig(ApprovalOutcome::Approved(ApprovalScope::Once));
        let long = format!("{}{}", "a".repeat(300), "SECOND PART");
        let text = "first line\nsecond line: \"quoted\"";
        assert!(call(&host, "flows.run", json!({"flow_id": 1, "inputs": {"city": long, "count": 3}})).ok);
        assert!(call(&host, "flows.run", json!({"flow_id": 1, "inputs": {"city": text}})).ok);
        let seen = ask.0.lock().unwrap();
        // The sentence is as it was (cut, and says by how much); the preview holds the rest.
        assert!(seen[0].summary.contains("(+191 more characters)") && !seen[0].summary.contains("SECOND PART"), "{}", seen[0].summary);
        let preview = seen[0].preview.clone().expect("the question carries the inputs");
        assert!(preview.starts_with("Inputs of the flow \"Morning report\":") && preview.contains("SECOND PART") && preview.contains("count = 3"), "{preview}");
        assert!(!preview.contains("more characters not shown"), "an input under the limit is whole: {preview}");
        // A value with a line break stays one line of the preview, between quotes.
        let second = seen[1].preview.clone().unwrap();
        assert!(second.contains(r#"city = "first line\nsecond line: \"quoted\"""#) && second.lines().count() == 2, "{second}");
        drop(seen);
        // An input over the limit is cut and says how much is not shown.
        let (host, _, ask) = rig(ApprovalOutcome::Approved(ApprovalScope::Once));
        assert!(call(&host, "flows.run", json!({"flow_id": 1, "inputs": {"city": "b".repeat(2600)}})).ok);
        let preview = ask.0.lock().unwrap()[0].preview.clone().unwrap();
        assert!(preview.contains("more characters not shown") && !preview.contains(&"b".repeat(2001)), "{}", preview.chars().take(120).collect::<String>());
        // A flow that takes no input has nothing more to show than its name, which the sentence has.
        let (host, _, ask) = rig(ApprovalOutcome::Approved(ApprovalScope::Once));
        assert!(call(&host, "flows.run", json!({"flow_id": 2})).ok);
        assert_eq!(ask.0.lock().unwrap()[0].preview, None);
    }

    #[test]
    fn a_run_reports_its_state_and_what_it_stopped_with_cleaned_to_one_line() {
        let (host, _, _) = rig(ApprovalOutcome::Denied);
        let res = call(&host, "flows.status", json!({"run_id": 77}));
        assert!(res.ok, "{}", res.text);
        assert!(res.text.contains("Run 77 of \"Report\": Failed.") && res.text.contains("It stopped with: step 3 failed: the page said NO") && res.text.contains("It took 12 s."), "{}", res.text);
        assert_eq!(res.data["seconds"], 12);
        assert_eq!(call(&host, "flows.status", json!({"run_id": 5})).error.unwrap().code, ErrorCode::NotFound);
        let manifests = host.manifests();
        assert!(manifests.iter().all(|m| m.untrusted_output) && manifests.iter().find(|m| m.name == "flows.run").unwrap().classes == vec![PermClass::ExternalEffect]);
    }
}
