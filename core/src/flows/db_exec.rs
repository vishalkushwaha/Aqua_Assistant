//! The "Database" step: one read-only query, run by the guarded tool host's `db.query` (card R7-12).
//!
//! It is the same door as the "Run a tool" step: the host checks the query (only SELECT, WITH, VALUES and EXPLAIN, one statement, on a connection that cannot write), decides, masks
//! the columns that hold secrets, and writes the audit line. A flow runs with nobody to ask, so a call the host would ask about is refused at once, and the step fails with the host's own
//! reason. Writes (`db.execute`, `db.command`) are not part of this step.
//!
//! What goes into the query. The query text is never filled in from `{{ }}` (the engine leaves it as written, and the checker refuses a `{{` in it): a value reaches the database only as one
//! of the step's "Values", which the tool binds to the `?` marks. So text that came from a page, a step or a webhook can be a value, and can never be part of the SQL.

use crate::workflows::engine::{Executor as ToolRunner, Outcome};
use crate::flows::run::exec::{NodeCtx, NodeError, NodeResult};
use serde_json::{json, Map, Value};
use std::time::Duration;

/// How long the host may take over one query, and the most rows it may be asked for (the tool's own cap).
const TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_ROWS: u64 = 1000;

fn text(cfg: &Value, key: &str) -> String {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
}

fn invalid(why: &str) -> NodeError {
    NodeError::Fatal(format!("invalid setting: {why}"))
}

/// The arguments of `db.query` for a step, from its settings (already filled in, except the query, which is the text as written).
pub fn query_arguments(cfg: &Value) -> Result<Value, NodeError> {
    let query = text(cfg, "query");
    if query.is_empty() {
        return Err(invalid("the query is empty"));
    }
    if query.contains("{{") {
        return Err(invalid("a query cannot have {{ }} in it; write ? where a value goes and give the value under \"Values\""));
    }
    let mut args = json!({ "sql": query });
    let profile = text(cfg, "profile");
    if !profile.is_empty() {
        args["profile"] = json!(profile);
    }
    let params = match cfg.get("params") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) if s.trim().is_empty() => Vec::new(),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(a)) => a,
            _ => return Err(invalid("\"Values\" must be a JSON list, like [\"a\", 5]")),
        },
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err(invalid("\"Values\" must be a JSON list, like [\"a\", 5]")),
    };
    if params.iter().any(|p| matches!(p, Value::Array(_) | Value::Object(_))) {
        return Err(invalid("each of the \"Values\" must be text, a number, true/false or empty, not a list or a record (name the field you mean, for example {{nodes.each.item.id}})"));
    }
    if !params.is_empty() {
        args["params"] = Value::Array(params);
    }
    match cfg.get("max_rows") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s.trim().is_empty() => {}
        Some(v) => match v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())) {
            Some(n) => args["max_rows"] = json!(n.clamp(1, MAX_ROWS)),
            None => return Err(invalid("\"Most rows\" must be a whole number")),
        },
    }
    Ok(args)
}

/// What the step hands on from the tool's answer: the rows (as many as the tool returned, which is capped), how many that is, and whether there were more.
pub fn rows_result(o: &Outcome) -> NodeResult {
    let rows = o.data.get("rows").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let count = rows.len();
    let truncated = o.data.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false);
    let from = o.data.get("profile").and_then(|v| v.as_str()).filter(|p| !p.is_empty()).map(|p| format!(" from {p}")).unwrap_or_default();
    let log = format!("Read {count} row{}{from}{}", if count == 1 { "" } else { "s" }, if truncated { "; more rows exist than were returned" } else { "" });
    let mut outputs = Map::new();
    outputs.insert("rows".into(), Value::Array(rows));
    outputs.insert("count".into(), json!(count));
    outputs.insert("truncated".into(), json!(truncated));
    NodeResult { outputs, logs: vec![log] }
}

/// Run a Database step through `tools` (the tool host), as a step of the run named `run_key` for the host.
pub fn run_query(tools: &dyn ToolRunner, run_key: &str, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
    let args = query_arguments(&ctx.config)?;
    let o = tools.run_tool_in(run_key, "db.query", &args, TIMEOUT, ctx.cancel);
    if !o.ok {
        // A read can be tried again whenever the host says it might help (a busy database); a refusal cannot.
        return Err(if o.retryable { NodeError::Retryable(o.error) } else { NodeError::Fatal(o.error) });
    }
    Ok(rows_result(&o))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_of_the_query_are_the_text_as_written_the_database_the_values_and_a_capped_row_limit() {
        let a = query_arguments(&json!({ "query": "  SELECT name FROM users WHERE id = ?  ", "profile": "shop", "params": ["{{x}}"], "max_rows": 5000 })).unwrap();
        // The tool's own names, the query trimmed, and the cap of the tool (1000) applied.
        assert_eq!(a, json!({ "sql": "SELECT name FROM users WHERE id = ?", "profile": "shop", "params": ["{{x}}"], "max_rows": 1000 }));
        // Nothing else is added when nothing else is set.
        assert_eq!(query_arguments(&json!({ "query": "SELECT 1", "profile": "", "params": "", "max_rows": null })).unwrap(), json!({ "sql": "SELECT 1" }));
        // Values may be written as JSON text, and a row limit as text.
        assert_eq!(query_arguments(&json!({ "query": "SELECT ?", "params": "[1, \"two\", null, true]", "max_rows": " 7 " })).unwrap(), json!({ "sql": "SELECT ?", "params": [1, "two", null, true], "max_rows": 7 }));
        assert_eq!(query_arguments(&json!({ "query": "SELECT 1", "max_rows": 0 })).unwrap()["max_rows"], json!(1), "at least one row");
    }

    #[test]
    fn a_query_with_double_braces_or_values_that_are_not_plain_or_a_bad_limit_is_refused_before_the_host_is_asked() {
        let fatal = |cfg: Value| match query_arguments(&cfg) {
            Err(NodeError::Fatal(m)) => m,
            other => panic!("{cfg}: {other:?}"),
        };
        assert!(fatal(json!({})).contains("the query is empty") && fatal(json!({ "query": "  " })).contains("the query is empty"));
        assert!(fatal(json!({ "query": "SELECT * FROM t WHERE a = '{{nodes.x.out}}'" })).contains("cannot have {{ }}"));
        assert!(fatal(json!({ "query": "SELECT ?", "params": "not json" })).contains("must be a JSON list"));
        assert!(fatal(json!({ "query": "SELECT ?", "params": {"a": 1} })).contains("must be a JSON list"));
        assert!(fatal(json!({ "query": "SELECT ?", "params": [{"id": 1}] })).contains("not a list or a record"));
        assert!(fatal(json!({ "query": "SELECT ?", "params": [[1, 2]] })).contains("not a list or a record"));
        assert!(fatal(json!({ "query": "SELECT 1", "max_rows": "many" })).contains("whole number"));
    }

    #[test]
    fn the_rows_the_tool_returns_are_handed_on_with_their_count_and_a_note_when_there_were_more() {
        let rows = json!([{ "id": 1, "name": "ann" }, { "id": 2, "name": "bob" }]);
        let r = rows_result(&Outcome::success("2 row(s)", json!({ "profile": "shop", "columns": ["id", "name"], "rows": rows, "row_count": 2, "truncated": true })));
        assert_eq!((r.outputs["rows"].clone(), r.outputs["count"].clone(), r.outputs["truncated"].clone()), (rows, json!(2), json!(true)));
        assert_eq!(r.logs, vec!["Read 2 rows from shop; more rows exist than were returned".to_string()]);
        // An answer with no rows in it is no rows, not an error.
        let empty = rows_result(&Outcome::success("0 row(s)", json!({})));
        assert_eq!((empty.outputs["rows"].clone(), empty.outputs["count"].clone(), empty.outputs["truncated"].clone()), (json!([]), json!(0), json!(false)));
        assert_eq!(empty.logs, vec!["Read 0 rows".to_string()]);
        assert_eq!(rows_result(&Outcome::success("", json!({ "rows": [{ "a": 1 }] }))).logs, vec!["Read 1 row".to_string()]);
    }
}
