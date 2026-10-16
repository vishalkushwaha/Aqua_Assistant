//! Card R7-12: the Database step in a real run. The tool host is a stand-in that records what it is asked and answers what the test says, except in the last test,
//! which uses the real database tool on a real SQLite file.

use super::*;
use crate::flows::db_exec;
use crate::workflows::engine::{Executor as ToolRunner, Outcome};

/// What the tool host is asked (the run it is asked for, the tool, the arguments) and what it answers.
#[derive(Default)]
struct Tools {
    calls: Mutex<Vec<(String, String, Value)>>,
    answers: Mutex<VecDeque<Outcome>>,
}

impl Tools {
    fn answer(&self, o: Outcome) {
        self.answers.lock().unwrap().push_back(o);
    }
    fn calls(&self) -> Vec<(String, String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ToolRunner for Tools {
    fn run_tool(&self, tool: &str, arguments: &Value, timeout: Duration, cancel: &AtomicBool) -> Outcome {
        self.run_tool_in("", tool, arguments, timeout, cancel)
    }
    fn run_tool_in(&self, run_key: &str, tool: &str, arguments: &Value, _timeout: Duration, _cancel: &AtomicBool) -> Outcome {
        self.calls.lock().unwrap().push((run_key.to_string(), tool.to_string(), arguments.clone()));
        self.answers.lock().unwrap().pop_front().unwrap_or_else(|| Outcome::failure("no answer was scripted", false))
    }
    fn run_agent(&self, _goal: &str, _max_steps: u32, _timeout: Duration, _cancel: &AtomicBool) -> Outcome {
        Outcome::failure("no agent here", false)
    }
}

/// The Database step, run the way the live executor runs it, over a tool host.
struct DbStep(Arc<dyn ToolRunner>);

impl NodeExecutor for DbStep {
    fn kinds(&self) -> &[NodeKind] {
        &[NodeKind::Database]
    }
    fn execute(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
        db_exec::run_query(self.0.as_ref(), &format!("flow_run_{}", ctx.run_id), ctx)
    }
}

fn db_rig(name: &str, tools: Arc<dyn ToolRunner>) -> Rig {
    let step: Arc<dyn NodeExecutor> = Arc::new(DbStep(tools));
    rig_with(name, vec![step])
}

fn rows_answer(profile: &str, rows: Value, truncated: bool) -> Outcome {
    let count = rows.as_array().map(|a| a.len()).unwrap_or(0);
    Outcome::success(format!("{count} row(s)"), json!({ "profile": profile, "columns": ["name"], "rows": rows, "row_count": count, "truncated": truncated }))
}

fn query_flow(r: &Rig, db: Value) -> i64 {
    r.flow(json!({ "name": "Query", "variables": [{ "name": "id", "label": "Id", "kind": "text", "required": false, "default": "7" }, { "name": "database", "label": "Database", "kind": "text", "required": false, "default": "shop" }],
        "nodes": [trigger(), db, notify("tell", "{{nodes.db.count}} rows, more: {{nodes.db.truncated}}")], "edges": [["start", "db"], ["db", "tell"]] }))
}

#[test]
fn a_database_step_asks_the_tool_host_for_one_query_with_its_values_bound_and_hands_on_the_rows_their_count_and_whether_there_were_more() {
    let tools = Arc::new(Tools::default());
    let r = db_rig("db_rows", tools.clone());
    tools.answer(rows_answer("shop", json!([{ "name": "a" }, { "name": "b" }, { "name": "c" }]), true));
    let f = query_flow(&r, n("db", "database", json!({ "profile": "shop", "query": "SELECT name FROM users WHERE id = ?", "params": ["{{vars.id}}"], "max_rows": 3 })));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    let calls = tools.calls();
    assert_eq!(calls.len(), 1, "one query, one call");
    assert_eq!((calls[0].0.clone(), calls[0].1.clone()), (format!("flow_run_{}", run.id), "db.query".to_string()), "a step of this run, for the tool host");
    assert_eq!(calls[0].2, json!({ "sql": "SELECT name FROM users WHERE id = ?", "profile": "shop", "params": ["7"], "max_rows": 3 }), "the query as written, the value bound");
    let db = r.node(run.id, "db");
    assert_eq!((db.outputs["count"].clone(), db.outputs["truncated"].clone(), db.outputs["rows"].as_array().map(|a| a.len())), (json!(3), json!(true), Some(3)));
    assert!(r.messages(run.id).iter().any(|m| m == "3 rows, more: true"), "{:?}", r.messages(run.id));
    assert!(r.events(run.id).iter().any(|e| e.kind == "log" && e.message == "Read 3 rows from shop; more rows exist than were returned"), "{:?}", r.messages(run.id));
    // Reading asks nobody and is not in the ledger: it may be done again.
    assert!(r.runs.effect_get(&format!("{}:db", run.id)).unwrap().is_none());
    assert!(r.runs.inbox("local").unwrap().is_empty());
}

#[test]
fn a_query_the_tool_host_refuses_stops_the_step_with_the_hosts_own_reason_and_a_busy_database_is_tried_again() {
    let tools = Arc::new(Tools::default());
    let r = db_rig("db_refused", tools.clone());
    tools.answer(Outcome::failure("approval is required for this call (a workflow runs with nobody to approve it: allow this action beforehand in the tool policy)", false));
    let f = query_flow(&r, n("db", "database", json!({ "query": "SELECT 1" })));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.contains("\"db\" failed: approval is required for this call") && run.error.contains("nobody to approve it"), "{}", run.error);
    assert_eq!((tools.calls().len(), r.node(run.id, "db").status, r.node(run.id, "tell").status), (1, NodeStatus::Failed, NodeStatus::Skipped), "a refusal is final: asked once");

    // A busy database is the host's word for "try again": the step's own retries are used.
    let tools = Arc::new(Tools::default());
    let r = db_rig("db_busy", tools.clone());
    tools.answer(Outcome::failure("the database is busy", true));
    tools.answer(rows_answer("shop", json!([{ "name": "a" }]), false));
    let mut db = n("db", "database", json!({ "query": "SELECT 1" }));
    db["retry"] = json!({ "retries": 1, "delay_secs": 10, "backoff": "fixed" });
    let f = query_flow(&r, db);
    let run = r.go_and_settle(f);
    assert_eq!((run.status, tools.calls().len(), r.node(run.id, "db").attempts), (RunStatus::Succeeded, 2, 2), "{:?}", r.messages(run.id));
    assert_eq!(*r.sleeps.lock().unwrap(), vec![10]);
}

#[test]
fn text_from_the_rows_of_a_for_each_reaches_the_query_only_as_a_bound_value_and_never_as_part_of_it() {
    let tools = Arc::new(Tools::default());
    let r = db_rig("db_bound", tools.clone());
    let evil = "x'); DROP TABLE users; --";
    r.fake.script("read", vec![Step::Ok(vec![("rows", json!([{ "name": "ann" }, { "name": evil }]))])]);
    tools.answer(rows_answer("", json!([]), false));
    tools.answer(rows_answer("", json!([]), false));
    let f = r.flow(json!({ "name": "Lookup", "nodes": [trigger(), n("read", "excel", json!({ "operation": "read_rows", "path": "people.xlsx" })), n("each", "loop", json!({ "items": "{{nodes.read.rows}}" })),
        n("db", "database", json!({ "query": "SELECT body FROM notes WHERE title = ?", "params": ["{{nodes.each.item.name}}"] })), notify("after", "{{nodes.each.count}} looked up")],
        "edges": [["start", "read"], ["read", "each"], ["each", "db", "each"], ["each", "after", "done"]] }));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    let calls = tools.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].2, json!({ "sql": "SELECT body FROM notes WHERE title = ?", "params": ["ann"] }));
    assert_eq!(calls[1].2, json!({ "sql": "SELECT body FROM notes WHERE title = ?", "params": [evil] }), "the text is a value; the SQL is the same text as for the first row");
    assert!(r.messages(run.id).iter().any(|m| m == "2 looked up"));
    assert_eq!(r.node(run.id, "each").outputs["results"].as_array().map(|a| a.len()), Some(2));
}

#[test]
fn a_query_with_double_braces_or_one_that_starts_with_a_write_is_refused_before_a_run_starts() {
    let tools = Arc::new(Tools::default());
    let r = db_rig("db_refused_early", tools.clone());
    for (query, why) in [
        ("SELECT * FROM notes WHERE title = '{{nodes.read.name}}'", "the query cannot have {{ }} in it"),
        ("DELETE FROM users WHERE id = 1", "this step only reads"),
        ("  -- tidy up\n drop table users", "\"DROP\" would be refused"),
    ] {
        let d = def(json!({ "name": "Bad", "nodes": [trigger(), n("db", "database", json!({ "query": query }))], "edges": [["start", "db"]] }));
        let f = r.flows.create("local", d, "user", "", 100).unwrap().id;
        let e = r.runner.start("local", f, Map::new(), Value::Null, false, "manual").unwrap_err();
        assert!(e.contains("has problems") && e.contains(why), "{query:?}: {e}");
    }
    assert!(tools.calls().is_empty(), "the host was not asked");
    // The values must be plain: a record is not one.
    let d = def(json!({ "name": "Bad values", "nodes": [trigger(), n("db", "database", json!({ "query": "SELECT ?", "params": [{ "id": 1 }] }))], "edges": [["start", "db"]] }));
    let report = crate::flows::validate::validate(&d);
    assert!(report.issues.iter().any(|i| i.code == "bad_config" && i.message.contains("not a list or a record")), "{:#?}", report.issues);
}

#[test]
fn the_database_cannot_be_named_by_text_from_outside_but_can_be_chosen_by_an_input() {
    let tools = Arc::new(Tools::default());
    let r = db_rig("db_profile", tools.clone());
    // Text that came from a step could name any database that is set up.
    let f = r.flow(json!({ "name": "Named by a page", "nodes": [trigger(), n("find", "research", json!({ "topic": "x" })), n("db", "database", json!({ "profile": "{{nodes.find.summary}}", "query": "SELECT 1" }))], "edges": [["start", "find"], ["find", "db"]] }));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.contains("\"db\" failed: the name of the database cannot be made from text that came from a page, a step or a webhook"), "{}", run.error);
    assert!(tools.calls().is_empty(), "nothing was asked");
    // An input of the flow is the person's own choice.
    tools.answer(rows_answer("shop", json!([{ "name": "a" }]), false));
    let f = query_flow(&r, n("db", "database", json!({ "profile": "{{vars.database}}", "query": "SELECT 1" })));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!(tools.calls()[0].2["profile"], "shop");
}

/// The real tool: `db.query` on a SQLite file, through the tool host, with the rows feeding a For each step.
struct RealHost(aqua_tool_host::ToolHost);

impl ToolRunner for RealHost {
    fn run_tool(&self, tool: &str, arguments: &Value, _timeout: Duration, _cancel: &AtomicBool) -> Outcome {
        use aqua_tool_host::{Caller, RequestContext, ToolRequest};
        let mut ctx = RequestContext::new(Caller::Agent);
        ctx.agent_run_id = "flow_run_test".into();
        // A flow runs with nobody to ask, as the live executor says.
        ctx.unattended = true;
        ctx.no_one_to_ask = true;
        let res = self.0.call_blocking(ToolRequest { tool: tool.into(), arguments: arguments.clone(), context: ctx });
        if res.ok {
            Outcome::success(res.text, res.data)
        } else {
            Outcome::failure(res.error.map(|e| e.message).unwrap_or(res.text), false)
        }
    }
    fn run_agent(&self, _goal: &str, _max_steps: u32, _timeout: Duration, _cancel: &AtomicBool) -> Outcome {
        Outcome::failure("no agent here", false)
    }
}

#[test]
fn against_the_real_database_tool_the_step_reads_rows_masks_credentials_caps_them_feeds_a_for_each_and_cannot_write() {
    use aqua_native_tools::db::{DbConfig, DbProfile, DbProvider, Env, Mode as DbMode};
    use aqua_native_tools::fs::{FsConfig, PathGuard};
    use aqua_native_tools::secrets::MemorySecrets;
    use aqua_tool_host::{AuditLedger, Mode, PolicyEngine, ToolHost};

    let dir = std::env::temp_dir().join(format!("aqua_flow_db_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["ws", "aqua"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let file = dir.join("ws").join("shop.db");
    {
        let c = rusqlite::Connection::open(&file).unwrap();
        c.execute_batch(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL, password TEXT);
             INSERT INTO users(name, password) VALUES ('ann', 'hunter2-ann'), ('bob', 'hunter2-bob'), ('cy', 'hunter2-cy');",
        )
        .unwrap();
    }
    let profile = DbProfile { alias: "shop".into(), kind: "sqlite".into(), path: file.to_string_lossy().to_string(), host: None, port: None, database: None, user: None, secret: None, tls: None, mode: DbMode::Ro, env: Env::Dev, trusted: false, max_rows: 200, max_write_rows: 100 };
    let guard = Arc::new(PathGuard::new(FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string()), vec![dir.join("aqua")]).unwrap());
    let provider = DbProvider::new(DbConfig { profiles: vec![profile] }, guard, Arc::new(MemorySecrets::new())).unwrap();
    let host = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("aqua").join("audit.jsonl"))));
    host.register(Arc::new(provider)).unwrap();
    let r = db_rig("db_real", Arc::new(RealHost(host)));

    // Two of the three rows are asked for; each is greeted by name.
    let f = r.flow(json!({ "name": "Greet", "nodes": [trigger(), n("db", "database", json!({ "profile": "shop", "query": "SELECT id, name, password FROM users ORDER BY id", "max_rows": 2 })), n("each", "loop", json!({ "items": "{{nodes.db.rows}}" })),
        notify("say", "hello {{nodes.each.item.name}}"), notify("after", "{{nodes.each.count}} greeted, more rows: {{nodes.db.truncated}}")],
        "edges": [["start", "db"], ["db", "each"], ["each", "say", "each"], ["each", "after", "done"]] }));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    let said: Vec<String> = r.messages(run.id).into_iter().filter(|m| m.starts_with("hello ")).collect();
    assert_eq!(said, ["hello ann", "hello bob"], "the cap of two rows held");
    assert!(r.messages(run.id).iter().any(|m| m == "2 greeted, more rows: true"), "{:?}", r.messages(run.id));
    let rows = r.node(run.id, "db").outputs["rows"].clone();
    assert_eq!(rows[0]["password"], "[REDACTED]", "a column named like a credential is masked by the tool");
    assert!(!serde_json::to_string(&r.node(run.id, "db").outputs).unwrap().contains("hunter2"), "and the value never reaches the flow");

    // A write hidden in a query that starts like a read passes the checker's first-word look and is refused by the tool, with the tool's own words. Nothing changes.
    let g = r.flow(json!({ "name": "Sneaky", "nodes": [trigger(), n("db", "database", json!({ "profile": "shop", "query": "WITH x AS (SELECT 1) DELETE FROM users WHERE id = 1" })), notify("tell", "{{nodes.db.count}}")], "edges": [["start", "db"], ["db", "tell"]] }));
    let run = r.go_and_settle(g);
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.contains("a read-only query may not contain 'delete'"), "{}", run.error);
    let left: i64 = rusqlite::Connection::open(&file).unwrap().query_row("SELECT count(*) FROM users", [], |x| x.get(0)).unwrap();
    assert_eq!(left, 3, "no row was deleted");
    let _ = std::fs::remove_dir_all(&dir);
}
