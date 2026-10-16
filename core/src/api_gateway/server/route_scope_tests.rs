//! Card OA3-01 (AQ-04). Four tests here used to search this file's own source for the call they expected (a handler that asks for the UI token, a route wired to a handler, the
//! agent stream that reduces what it sends): they broke, or passed without proving anything, when the file was split. A route of the real route table (`api_routes`) is asked now,
//! through the real token check (`require_local_auth`), with a gateway state over a throw-away folder (`test_support::app_state`): no model, no engine and no process is started.
//! The tests are plain ones with a runtime of their own, because the tool host's blocking call is not made from an async worker thread.
use super::*;
use crate::test_support::{app_state, wait_until, TempWorld};
use aqua_mcp_adapters::browser::{asking_for_run, ConfirmKind};
use aqua_tool_host::{Annotations, AuditLedger, Caller, Classification, ExecCtx, Mode, PermClass, PolicyEngine, ProviderFuture, ProviderOutput, RequestContext, ToolError, ToolHost, ToolManifest, ToolProvider, ToolRequest};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::time::Duration;
use tower::ServiceExt;

const API: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const UI: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A host with one read tool, `echo.read`, that answers "ran".
struct Reader;
impl ToolProvider for Reader {
    fn id(&self) -> &str {
        "reader"
    }
    fn manifests(&self) -> Vec<ToolManifest> {
        vec![ToolManifest {
            name: "echo.read".into(),
            title: "echo.read".into(),
            description: "Says that it ran.".into(),
            classes: vec![PermClass::Read],
            default_decision: aqua_tool_host::Decision::Auto,
            input_schema: json!({"type": "object"}),
            timeout_default_ms: 5_000,
            timeout_max_ms: 5_000,
            annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: false },
            untrusted_output: false,
        }]
    }
    fn classify(&self, tool: &str, _a: &Value) -> Result<Classification, ToolError> {
        Ok(Classification { classes: vec![PermClass::Read], target: "x".into(), summary: tool.into(), target_allowed: true })
    }
    fn call<'a>(&'a self, _tool: &'a str, _a: Value, _c: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        Box::pin(async { Ok(ProviderOutput { text: "ran".into(), ..Default::default() }) })
    }
}

fn host_in(world: &TempWorld) -> Arc<ToolHost> {
    let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(&world.path("host.audit.jsonl")))));
    host.register(Arc::new(Reader)).unwrap();
    host
}

/// The route table that is served, behind the check of the token, over a state of the test's own.
fn gateway(world: &TempWorld, tools: Arc<ToolHost>) -> (Router, AppState) {
    let state = app_state(world, tools);
    let auth = ApiAuth::new(API.to_string(), 8767).with_ui_token(UI.to_string());
    (api_routes().layer(axum::middleware::from_fn_with_state(auth, require_local_auth)).with_state(state.clone()), state)
}

fn run<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

async fn ask(app: &Router, method: &str, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = axum::http::Request::builder().method(method).uri(path).header("host", "127.0.0.1:8767");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app.clone().oneshot(request.body(axum::body::Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = http_body_util::BodyExt::collect(response.into_body()).await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Card R6-12: the lines of the ledger and the questions waiting for an answer show what the agent did and what it is about to do, and they answered anybody holding the API
/// token. Each handler has to check the scope itself, and `run` keeps one run's.
#[test]
fn the_ledger_lines_and_the_waiting_questions_need_the_ui_token_and_take_a_run() {
    let world = TempWorld::new("route_audit_scope");
    let host = host_in(&world);
    for run in ["run_one", "run_two"] {
        let mut context = RequestContext::new(Caller::Agent);
        context.agent_run_id = run.into();
        assert!(host.call_blocking(ToolRequest { tool: "echo.read".into(), arguments: json!({}), context }).ok);
    }
    let (app, state) = gateway(&world, host);
    // Two questions are open, one about each run (each asked on a thread of its own, as a run asks).
    let askers: Vec<_> = ["run_one", "run_two"]
        .into_iter()
        .map(|run| {
            let browser = state.browser.clone();
            std::thread::spawn(move || {
                let _about = asking_for_run(run);
                browser.broker().request(ConfirmKind::Tool, "term.run", "run a command".into(), None, None, Duration::from_secs(60)).1
            })
        })
        .collect();
    wait_until(Duration::from_secs(10), "both questions to be open", || state.browser.broker().pending().len() == 2);
    crate::runtime::data_tripwire::with_workspace_root(&world.root, || {
        run(async {
            for path in ["/api/v1/tools/audit/recent", "/api/v1/browser/confirmations"] {
                let (status, body) = ask(&app, "GET", path, Some(API)).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{path} answered the API token: {body}");
                assert_eq!(ask(&app, "GET", path, None).await.0, StatusCode::UNAUTHORIZED, "{path} with no token");
            }
            // The ledger: every line, or the lines of one run.
            let runs = |body: &Value| -> Vec<String> { body["records"].as_array().unwrap().iter().filter_map(|r| r["agent_run_id"].as_str().map(str::to_string)).collect() };
            let (status, all) = ask(&app, "GET", "/api/v1/tools/audit/recent", Some(UI)).await;
            assert_eq!(status, StatusCode::OK);
            assert!(runs(&all).contains(&"run_one".to_string()) && runs(&all).contains(&"run_two".to_string()), "{all}");
            let (_, one) = ask(&app, "GET", "/api/v1/tools/audit/recent?run=run_one", Some(UI)).await;
            assert!(!runs(&one).is_empty() && runs(&one).iter().all(|r| r == "run_one"), "{one}");
            // The questions: both, or the ones about one run.
            let (_, both) = ask(&app, "GET", "/api/v1/browser/confirmations", Some(UI)).await;
            assert_eq!(both["pending"].as_array().map(Vec::len), Some(2), "{both}");
            let (_, two) = ask(&app, "GET", "/api/v1/browser/confirmations?run=run_two", Some(UI)).await;
            let pending = two["pending"].as_array().unwrap();
            assert!(pending.len() == 1 && pending[0]["run_id"] == "run_two", "{two}");
            // The route that says whether the ledger holds is not a secret: it stays on the API token, so that a check can be run without the UI's.
            let (status, verify) = ask(&app, "GET", "/api/v1/tools/audit/verify", Some(API)).await;
            assert_eq!((status, verify["ok"].clone()), (StatusCode::OK, json!(true)), "{verify}");
        });
    });
    // Both questions are answered no, so that their threads end.
    for question in state.browser.broker().pending() {
        state.browser.broker().resolve_typed(&question.request_id, Decision::Deny);
    }
    for asker in askers {
        assert_eq!(asker.join().unwrap(), Decision::Deny);
    }
}

/// A POST with a JSON body.
async fn post_json(app: &Router, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:8767")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = http_body_util::BodyExt::collect(response.into_body()).await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Card OA4-01 (AG-02): the `profile` of an agent request is one of `coding`, `files`, `web` and `desktop`; any other word is a 400 that says so, from both agent routes, before a turn is taken
/// or anything is started (the route table is the real one, behind the real token check; no model or tool is touched, because the request is refused first).
#[test]
fn a_profile_that_is_not_one_of_the_four_words_is_refused_with_a_400_by_both_agent_routes() {
    let world = TempWorld::new("route_agent_profile");
    let (app, _) = gateway(&world, host_in(&world));
    run(async {
        for path in ["/api/v1/mcp/agent", "/api/v1/mcp/agent/stream"] {
            for bad in ["code", "", "coding,web", "CODING FILES", "0", "\u{1b}[31mred"] {
                let (status, body) = post_json(&app, path, API, json!({ "goal": "fix the failing test in rust_probe", "profile": bad })).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{path} with profile {bad:?}: {body}");
                let why = body["error"].as_str().unwrap_or_default();
                assert!(why.starts_with("profile is coding, files, web or desktop"), "{path}: {body}");
                assert!(!why.contains('\u{1b}'), "no control character is sent back: {why:?}");
            }
            // A profile that is not a string at all is the body's own fault: it is refused before the handler is reached (the answer is 422, or 400).
            let (status, _) = post_json(&app, path, API, json!({ "goal": "fix it", "profile": 7 })).await;
            assert!(status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST, "{path}: a number is not a profile: {status}");
            // No token: no answer about the field.
            let request = axum::http::Request::builder().method("POST").uri(path).header("host", "127.0.0.1:8767").header("content-type", "application/json").body(axum::body::Body::from(json!({ "goal": "x", "profile": "nope" }).to_string())).unwrap();
            assert_eq!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::UNAUTHORIZED, "{path} with no token");
        }
    });
}

/// Starting and stopping Browser MCP, Windows-MCP and the managed browser changes what Aqua can reach, so a caller holding only the API token must be turned away. Each handler
/// has to check the scope itself. (Only the refusal is asked: with the UI token these would start the things.)
#[test]
fn starting_and_stopping_tools_needs_the_ui_token() {
    let world = TempWorld::new("route_lifecycle_scope");
    let (app, _) = gateway(&world, host_in(&world));
    run(async {
        for path in ["/api/v1/browser-mcp/start", "/api/v1/browser-mcp/stop", "/api/v1/windows-mcp/start", "/api/v1/windows-mcp/stop", "/api/v1/browser/start", "/api/v1/browser/stop"] {
            let (status, body) = ask(&app, "POST", path, Some(API)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path} was served to the API token: {body}");
            assert_eq!(ask(&app, "POST", path, None).await.0, StatusCode::UNAUTHORIZED, "{path} with no token");
        }
    });
}

/// Card OA1-04 (SE-19): what the answer of the public route holds for a caller with no token is two fields, and `status` is the one the start-up scripts look at. The route is the
/// one that tells the two apart, and `status` is the one that always wants a token.
#[test]
fn health_for_a_caller_with_no_token_is_the_status_word_and_the_version_and_nothing_else() {
    let body = super::minimal_health();
    let mut keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["status", "version"]);
    assert_eq!(body["status"], "RUNNING");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    // The route is wired to the handler that tells the two apart, not to the one that always gives the whole status.
    let world = TempWorld::new("route_health");
    let (app, _) = gateway(&world, host_in(&world));
    crate::runtime::data_tripwire::with_workspace_root(&world.root, || {
        run(async {
            let (status, shown) = ask(&app, "GET", "/api/v1/health", None).await;
            assert_eq!((status, shown), (StatusCode::OK, body.clone()), "no token: the status word and the version");
            let (status, whole) = ask(&app, "GET", "/api/v1/health", Some(API)).await;
            assert_eq!(status, StatusCode::OK);
            assert!(whole.get("ram_sla_mb").is_some() && whole.get("models").is_some() && whole.get("llm_engine_ready").is_some(), "a token: the whole status: {whole}");
            let (status, _) = ask(&app, "GET", "/api/v1/status", None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "the status needs a token");
            let (status, whole) = ask(&app, "GET", "/api/v1/status", Some(API)).await;
            assert_eq!(status, StatusCode::OK);
            assert!(whole.get("ram_sla_mb").is_some(), "{whole}");
        });
    });
}

/// A tool of the terminal that only says what it was asked: a step of a command is a call of it, and the host's own answer (here: nobody to ask, so it is refused) is the command's.
struct Terminal;
impl ToolProvider for Terminal {
    fn id(&self) -> &str {
        "fake_term"
    }
    fn manifests(&self) -> Vec<ToolManifest> {
        vec![ToolManifest {
            name: "term.run".into(),
            title: "term.run".into(),
            description: "Pretends to run a program.".into(),
            classes: vec![PermClass::Execute],
            default_decision: aqua_tool_host::Decision::Auto,
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": true}),
            timeout_default_ms: 5_000,
            timeout_max_ms: 5_000,
            annotations: Annotations { read_only: false, destructive: false, idempotent: false, open_world: false },
            untrusted_output: true,
        }]
    }
    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        Ok(Classification { classes: vec![PermClass::Execute], target: "x".into(), summary: format!("{tool} {args}"), target_allowed: true })
    }
    fn call<'a>(&'a self, _tool: &'a str, _a: Value, _c: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        Box::pin(async { Ok(ProviderOutput { text: "printed".into(), ..Default::default() }) })
    }
}

async fn post(app: &Router, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:8767")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = http_body_util::BodyExt::collect(response.into_body()).await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Card OA11-02 (EX-08): a command is listed and started through the real route table. It is off unless `AQUA_COMMANDS=on`; a command that does not exist is not found; a step that is
/// refused ends the command before any agent starts, and a word that cannot be put in a step is refused before the host is asked at all.
#[test]
fn a_command_is_listed_and_started_through_the_route_table_and_a_refused_step_starts_nothing() {
    let world = TempWorld::new("route_commands");
    let host = host_in(&world);
    host.register(Arc::new(Terminal)).unwrap();
    let (app, _) = gateway(&world, host);
    let ledger = world.path("host.audit.jsonl");
    std::fs::create_dir_all(world.root.join("data/commands")).unwrap();
    std::fs::write(world.root.join("data/commands/review.md"), "---\ndescription: Review one file\nargument-hint: <file>\nallowed-tools: fs.read\n---\nReview $1.\n!`git diff -- $1`").unwrap();
    crate::runtime::data_tripwire::with_workspace_root(&world.root, || {
        run(async {
            // Off: the list is empty and says why; a command cannot be started.
            let (status, body) = ask(&app, "GET", "/api/v1/commands", Some(API)).await;
            assert_eq!((status, body["enabled"].clone(), body["commands"].clone()), (StatusCode::OK, json!(false), json!([])), "{body}");
            let (status, body) = post(&app, "/api/v1/commands/review/run", API, json!({"arguments": "src/lib.rs"})).await;
            assert_eq!((status, body["enabled"].clone()), (StatusCode::CONFLICT, json!(false)), "{body}");
            assert_eq!(ask(&app, "GET", "/api/v1/commands", None).await.0, StatusCode::UNAUTHORIZED);
        });
        crate::flags::with_overrides(&[("AQUA_COMMANDS", Some("on"))], || {
            run(async {
                let (status, body) = ask(&app, "GET", "/api/v1/commands", Some(API)).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                assert_eq!((body["commands"][0]["name"].clone(), body["commands"][0]["steps"].clone(), body["commands"][0]["source"].clone()), (json!("review"), json!(1), json!("data")), "{body}");
                assert!(!body.to_string().contains("git diff"), "a list holds no command text: {body}");
                // A command that is not there is not found.
                let (status, body) = post(&app, "/api/v1/commands/nothing/run", API, json!({})).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
                // A word that cannot go in a step is refused before the host is asked: nothing is in the ledger.
                for token in [API, UI] {
                    let (status, body) = post(&app, "/api/v1/commands/review/run", token, json!({"arguments": "\"x; rm -rf /\""})).await;
                    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
                    assert!(body["error"].as_str().unwrap().contains("cannot be put in a step"), "{body}");
                }
                assert!(!std::fs::read_to_string(&ledger).unwrap_or_default().contains("term.run"), "the host was never asked");
                // A step that the host does not run (nobody is there to answer) ends the command, and no agent is started: the answer is the refusal and which step.
                let (status, body) = post(&app, "/api/v1/commands/review/run", UI, json!({"arguments": "src/lib.rs"})).await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
                assert!(body["error"].as_str().unwrap().contains("step 1 of the command review was not run"), "{body}");
                assert!(std::fs::read_to_string(&ledger).unwrap_or_default().contains("term.run"), "the step was a call of the host");
                // A folder outside the folders Aqua works in is refused.
                let (status, body) = post(&app, "/api/v1/commands/review/run", UI, json!({"arguments": "a", "folder": std::env::temp_dir().to_string_lossy()})).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            });
        });
    });
}

/// Card OA11-04 (AG-05, EX-12): the agents are listed through the real route table, never with the words of their instructions; a request that asks for an agent is refused while agents
/// are off (409) or when the name is not a name (400) before any turn is taken; a command that names an agent is refused before any of its steps is run when agents are off.
#[test]
fn agents_are_listed_and_asked_for_through_the_route_table_and_are_refused_while_off() {
    let world = TempWorld::new("route_agents");
    let host = host_in(&world);
    host.register(Arc::new(Terminal)).unwrap();
    let (app, _) = gateway(&world, host);
    let ledger = world.path("host.audit.jsonl");
    std::fs::create_dir_all(world.root.join("data/agents")).unwrap();
    std::fs::write(world.root.join("data/agents/finder.md"), "---\ndescription: Looks for names\nmode: subagent\nallowed-tools: fs.grep\n---\nA SECRET WORD THAT IS THE AGENT'S INSTRUCTIONS").unwrap();
    std::fs::write(world.root.join("data/agents/broken.md"), "---\ntools: fs.read\n---\nx").unwrap();
    std::fs::create_dir_all(world.root.join("data/commands")).unwrap();
    std::fs::write(world.root.join("data/commands/review.md"), "---\ndescription: Review one file\nallowed-tools: fs.read\nagent: review\n---\nReview $1.\n!`git diff -- $1`").unwrap();
    crate::runtime::data_tripwire::with_workspace_root(&world.root, || {
        // Off (the default): the list is empty and says why, and every way to ask for an agent is refused with the reason.
        run(async {
            let (status, body) = ask(&app, "GET", "/api/v1/agents", Some(API)).await;
            assert_eq!((status, body["enabled"].clone(), body["agents"].clone()), (StatusCode::OK, json!(false), json!([])), "{body}");
            assert_eq!(ask(&app, "GET", "/api/v1/agents", None).await.0, StatusCode::UNAUTHORIZED);
            for path in ["/api/v1/mcp/agent", "/api/v1/mcp/agent/stream"] {
                let (status, body) = post(&app, path, API, json!({"goal": "read proj/calc.py", "agent": "review"})).await;
                assert_eq!((status, body["enabled"].clone()), (StatusCode::CONFLICT, json!(false)), "{path}: {body}");
            }
            // A name that is not a name is refused whether agents are on or not.
            let (status, body) = post(&app, "/api/v1/mcp/agent", API, json!({"goal": "read proj/calc.py", "agent": "Bad Name!"})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            // `POST /api/v1/runs` keeps its old answer for a name that is neither build nor plan.
            let (status, body) = post(&app, "/api/v1/runs", API, json!({"goal": "read proj/calc.py", "agent": "review"})).await;
            assert_eq!((status, body["error"].as_str().unwrap_or("")), (StatusCode::BAD_REQUEST, "agent is build or plan"), "{body}");
        });
        crate::flags::with_overrides(&[("AQUA_COMMANDS", Some("on"))], || {
            run(async {
                // A command that names an agent does not run while agents are off: none of its steps is run (the host is not asked), and the answer says why.
                let (status, body) = post(&app, "/api/v1/commands/review/run", UI, json!({"arguments": "src/lib.rs"})).await;
                assert_eq!((status, body["enabled"].clone()), (StatusCode::CONFLICT, json!(false)), "{body}");
                assert!(!std::fs::read_to_string(&ledger).unwrap_or_default().contains("term.run"), "no step was run");
            });
        });
        crate::flags::with_overrides(&[("AQUA_AGENTS", Some("on"))], || {
            run(async {
                let (status, body) = ask(&app, "GET", "/api/v1/agents", Some(API)).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                let names: Vec<&str> = body["agents"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect();
                assert_eq!(names, ["build", "plan", "explore", "review", "finder"], "{body}");
                let finder = body["agents"].as_array().unwrap().iter().find(|a| a["name"] == "finder").unwrap();
                assert_eq!((finder["source"].clone(), finder["mode"].clone(), finder["allowed_tools"].clone()), (json!("data"), json!("subagent"), json!(["fs.grep"])), "{finder}");
                assert!(!body.to_string().contains("SECRET WORD"), "a list holds no instructions: {body}");
                assert!(body["problems"].as_array().unwrap().iter().any(|p| p.as_str().unwrap().contains("agent broken") && p.as_str().unwrap().contains("allowed-tools")), "{body}");
                // A name that is not a name is still refused, and `runs` takes a valid name now (it is looked up when the run begins).
                let (status, _) = post(&app, "/api/v1/mcp/agent", API, json!({"goal": "read proj/calc.py", "agent": "../etc"})).await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
                let (status, body) = post(&app, "/api/v1/runs", API, json!({"goal": "read proj/calc.py", "agent": "Not Valid"})).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            });
        });
    });
}

/// Card OA11-02: the two options a command gives the run are checked at the door of the agent routes, so a request that carries them wrongly is refused before a turn is taken.
#[test]
fn the_options_a_request_gives_its_run_are_checked_before_the_run_starts() {
    let ok = super::run_options_of(&["fs.read".to_string(), "git.*".to_string()], &["files".to_string()], false).unwrap();
    assert_eq!((ok.allowed_tools, ok.forced_packs, ok.tainted), (vec!["fs.read".to_string(), "git.*".to_string()], vec!["files".to_string()], false));
    assert!(super::run_options_of(&["x".repeat(81)], &[], false).is_err());
    assert!(super::run_options_of(&["fs read".to_string()], &[], false).is_err());
    assert!(super::run_options_of(&[String::new()], &[], false).is_err());
    assert!(super::run_options_of(&vec!["fs.read".to_string(); 41], &[], false).is_err());
    assert!(super::run_options_of(&[], &vec!["files".to_string(); 9], false).is_err());
    assert!(super::run_options_of(&[], &["p".repeat(41)], false).is_err());
    assert!(super::run_options_of(&[], &[], true).unwrap().tainted);
}

/// Card OA6-05 (ED-10, decision D-08): an edit grant is made by the answer of the person in the app and by nothing else. The answer is `POST /api/v1/browser/confirm` with `scope: "run"`, a route
/// of the UI token: the API token is turned away (403) and the question stays open, so a script cannot make a grant. The option is taken only with `approve`, only on a question that offers it,
/// and only while the switch `AQUA_EDIT_GRANT` is on; anything else is refused and the question can still be answered. (The route table is the real one, behind the real token check.)
#[test]
fn a_grant_from_the_api_token_is_refused_and_the_run_option_is_taken_only_from_the_ui_token_on_a_question_that_offers_it_while_the_switch_is_on() {
    use aqua_mcp_adapters::browser::ConfirmWhy;
    let world = TempWorld::new("route_run_option");
    let (app, state) = gateway(&world, host_in(&world));
    let open = |offer: Option<&str>| -> (String, std::thread::JoinHandle<Decision>) {
        let browser = state.browser.clone();
        let why = ConfirmWhy { run_grant: offer.map(str::to_string), ..Default::default() };
        let asker = std::thread::spawn(move || browser.broker().request_explained(ConfirmKind::Tool, "fs.edit", "edit a.rs".into(), None, None, true, vec!["WRITE".into()], why, Duration::from_secs(60)).1);
        wait_until(Duration::from_secs(10), "the question to be open", || !state.browser.broker().pending().is_empty());
        (state.browser.broker().pending()[0].request_id.clone(), asker)
    };
    let offer = Some("Allow edits in src for this run: after this one, later edits of files in that folder go ahead until this run ends");
    let confirm = "/api/v1/browser/confirm";
    let on = crate::test_support::EnvGuard::set(&[("AQUA_EDIT_GRANT", "on")]);
    run(async {
        // The API token: refused, whatever it says, and the question is still there.
        let (id, asker) = open(offer);
        for body in [json!({"request_id": id, "decision": "approve", "scope": "run"}), json!({"request_id": id, "decision": "approve"}), json!({"request_id": id, "decision": "approve", "scope": "everything"})] {
            let (status, answer) = post_json(&app, confirm, API, body.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}: {answer}");
        }
        assert_eq!(state.browser.broker().pending().len(), 1, "a script did not answer it");
        assert!(!state.browser.broker().take_run_scope(&id));
        // The person, in the app: words that are not the option, and the option with the wrong answer, are refused and leave the question open.
        for (what, body) in [
            ("a scope that is not run", json!({"request_id": id, "decision": "approve", "scope": "everything"})),
            ("the option with a no", json!({"request_id": id, "decision": "deny", "scope": "run"})),
            ("the option with approve for good", json!({"request_id": id, "decision": "approve_always", "scope": "run"})),
            ("the option with words typed beside the yes", json!({"request_id": id, "decision": "approve", "scope": "run", "reason": "and delete the rest"})),
        ] {
            let (status, answer) = post_json(&app, confirm, UI, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {answer}");
            assert_eq!(state.browser.broker().pending().len(), 1, "{what}: still open");
        }
        // Then the option, on the question that offers it.
        let (status, answer) = post_json(&app, confirm, UI, json!({"request_id": id, "decision": "approve", "scope": "run"})).await;
        assert_eq!((status, answer["answer"].clone(), answer["scope"].clone()), (StatusCode::OK, json!("approve"), json!("run")), "{answer}");
        assert_eq!(asker.join().unwrap(), Decision::Allow, "it is a yes");
        assert!(state.browser.broker().take_run_scope(&id), "and the one who asked finds the mark");
        // A question that does not offer it: the option is a 400, the question can still be approved plainly, and no mark is left.
        let (id, asker) = open(None);
        let (status, answer) = post_json(&app, confirm, UI, json!({"request_id": id, "decision": "approve", "scope": "run"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
        assert!(answer["error"].as_str().unwrap().contains("does not offer the run option"), "{answer}");
        assert_eq!(post_json(&app, confirm, UI, json!({"request_id": id, "decision": "approve"})).await.0, StatusCode::OK);
        assert_eq!(asker.join().unwrap(), Decision::Allow);
        assert!(!state.browser.broker().take_run_scope(&id));
        // A question that is not there, or that was answered: what became of it, as for any answer.
        assert_eq!(post_json(&app, confirm, UI, json!({"request_id": "c_nothing", "decision": "approve", "scope": "run"})).await.0, StatusCode::NOT_FOUND);
        assert_eq!(post_json(&app, confirm, UI, json!({"request_id": id, "decision": "approve", "scope": "run"})).await.0, StatusCode::CONFLICT);
    });
    drop(on);
    // With the switch off the option is refused with a 409, and the question stays open for a plain answer.
    let _off = crate::test_support::EnvGuard::unset(&["AQUA_EDIT_GRANT"]);
    run(async {
        let (id, asker) = open(offer);
        let (status, answer) = post_json(&app, confirm, UI, json!({"request_id": id, "decision": "approve", "scope": "run"})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{answer}");
        assert!(answer["error"].as_str().unwrap().contains("AQUA_EDIT_GRANT=on"), "{answer}");
        assert_eq!(state.browser.broker().pending().len(), 1);
        assert_eq!(post_json(&app, confirm, UI, json!({"request_id": id, "decision": "deny"})).await.0, StatusCode::OK);
        assert_eq!(asker.join().unwrap(), Decision::Deny);
        assert!(!state.browser.broker().take_run_scope(&id));
    });
}
