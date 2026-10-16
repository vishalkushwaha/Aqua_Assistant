//! A run as spans: the shape `GET /api/v1/runs/{id}/trace` answers with and the exporter writes.
//!
//! One root span (`invoke_agent aqua`), and under it, for each planner reply, a `plan aqua` span (operation `plan`) with the model call that made it as its child (`chat {model}`,
//! operation `chat`, which carries the tokens and the timings), and for each tool call an `execute_tool {tool}` span. That is the shape of the OpenTelemetry GenAI conventions for an
//! agent's planning (card R6-37; they are still in "Development", and the names are in [`super::otel`] so that a change upstream is one edit).
//! The ids are derived from the run id, so asking twice gives the same trace. Times are Unix milliseconds.

use super::otel as o;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

fn hex(s: &str, len: usize) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..len].to_string()
}

pub fn trace_id(run_id: &str) -> String {
    hex(run_id, 32)
}

fn span_id(run_id: &str, seq: i64) -> String {
    hex(&format!("{run_id}#{seq}"), 16)
}

/// The id of the model call under a plan span.
fn model_call_id(run_id: &str, seq: i64) -> String {
    hex(&format!("{run_id}#{seq}#chat"), 16)
}

/// `run` is what `AgentRunStore::get` returns (with its `steps`). `now_ms` closes a span that has not ended.
pub fn spans(run: &Value, now_ms: i64) -> Vec<Value> {
    let run_id = run["id"].as_str().unwrap_or("");
    let start = run["started_at"].as_i64().unwrap_or(0);
    let end = run["ended_at"].as_i64().unwrap_or(now_ms).max(start);
    let status = run["status"].as_str().unwrap_or("");
    let root_id = span_id(run_id, 0);
    let mut root_attrs = Map::new();
    root_attrs.insert(o::OPERATION_NAME.into(), json!(o::OP_INVOKE_AGENT));
    root_attrs.insert(o::AGENT_NAME.into(), json!(o::AGENT));
    root_attrs.insert(o::AQUA_RUN_ID.into(), json!(run_id));
    root_attrs.insert(o::AQUA_RUN_STATUS.into(), json!(status));
    root_attrs.insert(o::AQUA_STOPPED_REASON.into(), run["stopped_reason"].clone());
    root_attrs.insert(o::AQUA_CALLER.into(), run["caller"].clone());
    root_attrs.insert(o::AQUA_ROUTE.into(), run["route"].clone());
    if run["model"].as_str().is_some_and(|m| !m.is_empty()) {
        root_attrs.insert(o::REQUEST_MODEL.into(), run["model"].clone());
    }
    let mut out = vec![json!({
        "span_id": root_id,
        "parent_span_id": Value::Null,
        "name": format!("{} {}", o::OP_INVOKE_AGENT, o::AGENT),
        "kind": "internal",
        "start_unix_ms": start,
        "end_unix_ms": end,
        "status": span_status(status),
        "attributes": root_attrs,
    })];
    let steps = run["steps"].as_array().cloned().unwrap_or_default();
    for s in &steps {
        let seq = s["seq"].as_i64().unwrap_or(0);
        let kind = s["kind"].as_str().unwrap_or("");
        let at = s["ts"].as_i64().unwrap_or(start);
        let ms = s["ms"].as_i64().unwrap_or(0).max(0);
        // A plan step is written when the reply arrives, so its span ends at `ts` and began `ms` earlier; a tool step is written
        // before the call, so it began at `ts`.
        let (from, to) = if kind == "plan" { ((at - ms).max(start), at) } else { (at, at + ms) };
        let st = match (s["status"].as_str().unwrap_or(""), s["ok"].as_bool()) {
            ("interrupted", _) => "error",
            (_, Some(false)) => "error",
            ("started", _) => "unset",
            _ => "ok",
        };
        let usage = |a: &mut Map<String, Value>| {
            for (key, field) in [
                (o::USAGE_INPUT_TOKENS, "tokens_in"),
                (o::USAGE_OUTPUT_TOKENS, "tokens_out"),
                (o::AQUA_CACHE_READ_TOKENS, "cache_read"),
                (o::AQUA_TTFT_MS, "ttft_ms"),
                (o::AQUA_DECODE_TPS, "decode_tokens_per_second"),
            ] {
                if !s[field].is_null() {
                    a.insert(key.into(), s[field].clone());
                }
            }
        };
        if kind == "plan" {
            // The planning phase, and the model call that made the plan under it: the tokens and the timings are the call's.
            let plan_id = span_id(run_id, seq);
            let mut plan = Map::new();
            plan.insert(o::OPERATION_NAME.into(), json!(o::OP_PLAN));
            plan.insert(o::AGENT_NAME.into(), json!(o::AGENT));
            plan.insert(o::AQUA_STEP.into(), s["step"].clone());
            out.push(json!({
                "span_id": plan_id,
                "parent_span_id": root_id,
                "name": format!("{} {}", o::OP_PLAN, o::AGENT),
                "kind": "internal",
                "start_unix_ms": from,
                "end_unix_ms": to.max(from),
                "status": st,
                "attributes": plan,
            }));
            let model = run["model"].as_str().unwrap_or("");
            let mut call = Map::new();
            call.insert(o::OPERATION_NAME.into(), json!(o::OP_CHAT));
            call.insert(o::AQUA_STEP.into(), s["step"].clone());
            if !model.is_empty() {
                call.insert(o::REQUEST_MODEL.into(), json!(model));
            }
            usage(&mut call);
            out.push(json!({
                "span_id": model_call_id(run_id, seq),
                "parent_span_id": plan_id,
                "name": if model.is_empty() { o::OP_CHAT.to_string() } else { format!("{} {model}", o::OP_CHAT) },
                "kind": "client",
                "start_unix_ms": from,
                "end_unix_ms": to.max(from),
                "status": st,
                "attributes": call,
            }));
            continue;
        }
        let mut a = Map::new();
        a.insert(o::AQUA_STEP.into(), s["step"].clone());
        let (name, op) = match kind {
            "tool" => {
                let tool = s["tool"].as_str().unwrap_or("");
                a.insert(o::TOOL_NAME.into(), json!(tool));
                a.insert(o::AQUA_DECISION.into(), s["decision"].clone());
                if s["approval"].as_str().is_some_and(|x| !x.is_empty()) {
                    a.insert(o::AQUA_APPROVAL.into(), s["approval"].clone());
                }
                (format!("execute_tool {tool}"), o::OP_EXECUTE_TOOL)
            }
            other => (other.to_string(), o::OP_CHAT),
        };
        a.insert(o::OPERATION_NAME.into(), json!(op));
        usage(&mut a);
        out.push(json!({
            "span_id": span_id(run_id, seq),
            "parent_span_id": root_id,
            "name": name,
            "kind": "internal",
            "start_unix_ms": from,
            "end_unix_ms": to.max(from),
            "status": st,
            "attributes": a,
        }));
    }
    out
}

fn span_status(run_status: &str) -> &'static str {
    match run_status {
        "completed" => "ok",
        "running" | "waiting_user" | "expired" => "unset",
        _ => "error",
    }
}

/// The answer of the trace route: the tool calls in order (`steps`, which the benchmark graders read) and the spans.
pub fn trace(run: &Value, now_ms: i64) -> Value {
    let run_id = run["id"].as_str().unwrap_or("");
    let steps: Vec<Value> = run["steps"]
        .as_array()
        .map(|a| a.iter().filter(|s| s["kind"] == "tool").cloned().collect())
        .unwrap_or_default();
    json!({
        "run_id": run_id,
        "trace_id": trace_id(run_id),
        "status": run["status"],
        "stopped_reason": run["stopped_reason"],
        "steps": steps,
        "spans": spans(run, now_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "id": "run_1_1", "started_at": 1_000, "ended_at": 1_900, "status": "completed", "stopped_reason": "final", "caller": "api", "route": "mcp_agent", "model": "qwen3-8b",
            "steps": [
                {"seq": 1, "step": 1, "kind": "plan", "tool": "", "status": "completed", "ok": true, "ms": 300, "ts": 1_300, "decision": "", "approval": "", "tokens_in": 812, "tokens_out": 24, "cache_read": 700},
                {"seq": 2, "step": 1, "kind": "tool", "tool": "fs.read", "status": "completed", "ok": true, "ms": 40, "ts": 1_310, "decision": "allowed", "approval": "", "tokens_in": null, "tokens_out": null, "cache_read": null},
                {"seq": 3, "step": 2, "kind": "tool", "tool": "fs.delete", "status": "failed", "ok": false, "ms": 5, "ts": 1_700, "decision": "denied", "approval": "denied", "tokens_in": null, "tokens_out": null, "cache_read": null},
                {"seq": 4, "step": 2, "kind": "final", "tool": "", "status": "completed", "ok": true, "ms": null, "ts": 1_890, "decision": "", "approval": "", "tokens_in": null, "tokens_out": null, "cache_read": null},
            ]
        })
    }

    #[test]
    fn a_run_is_one_agent_span_with_a_plan_span_and_its_model_call_for_each_reply_and_a_span_for_each_tool_call() {
        let t = trace(&sample(), 5_000);
        let spans = t["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 6);
        let names: Vec<&str> = spans.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["invoke_agent aqua", "plan aqua", "chat qwen3-8b", "execute_tool fs.read", "execute_tool fs.delete", "final"]);
        let (root, plan, call) = (&spans[0], &spans[1], &spans[2]);
        assert_eq!((root["attributes"]["gen_ai.operation.name"].as_str(), root["status"].as_str(), root["parent_span_id"].clone()), (Some("invoke_agent"), Some("ok"), Value::Null));
        // The planning phase is a span of its own, named and typed as the conventions say, and the model call that made the plan hangs under it.
        assert_eq!((plan["attributes"]["gen_ai.operation.name"].as_str(), plan["attributes"]["gen_ai.agent.name"].as_str(), plan["kind"].as_str()), (Some("plan"), Some("aqua"), Some("internal")));
        assert_eq!((call["attributes"]["gen_ai.operation.name"].as_str(), call["attributes"]["gen_ai.request.model"].as_str(), call["kind"].as_str()), (Some("chat"), Some("qwen3-8b"), Some("client")));
        assert_eq!((plan["parent_span_id"].clone(), call["parent_span_id"].clone()), (root["span_id"].clone(), plan["span_id"].clone()));
        assert!(spans[3..].iter().all(|s| s["parent_span_id"] == root["span_id"]), "every tool call and the end hang off the root, beside the plan");
        // A plan span ends when its reply arrived and began `ms` earlier, and so does its model call; a tool span begins when the call was written.
        for s in [plan, call] {
            assert_eq!((s["start_unix_ms"].as_i64(), s["end_unix_ms"].as_i64()), (Some(1_000), Some(1_300)));
        }
        assert_eq!((spans[3]["start_unix_ms"].as_i64(), spans[3]["end_unix_ms"].as_i64()), (Some(1_310), Some(1_350)));
        // The tokens and the timings are the model call's, not the planning phase's, and are counted once.
        assert_eq!(call["attributes"]["gen_ai.usage.input_tokens"], 812);
        assert!(plan["attributes"].get("gen_ai.usage.input_tokens").is_none() && plan["attributes"].get("aqua.usage.time_to_first_token_ms").is_none());
        assert_eq!(spans[4]["status"], "error", "a failed call is an error span");
        assert_eq!(spans[4]["attributes"]["aqua.tool.approval"], "denied");
        assert!(spans[3]["attributes"].get("aqua.tool.approval").is_none(), "no question, no approval attribute");
        let tokens: i64 = spans.iter().filter_map(|s| s["attributes"]["gen_ai.usage.input_tokens"].as_i64()).sum();
        assert_eq!(tokens, 812, "what the eval runner sums is the model's calls, once each");
    }

    #[test]
    fn a_plan_span_with_no_model_known_is_still_a_plan_with_a_call_named_chat_and_a_step_in_flight_is_unset() {
        let run = json!({"id": "r2", "started_at": 100, "ended_at": null, "status": "running", "steps": [
            {"seq": 1, "step": 1, "kind": "plan", "status": "started", "ok": null, "ms": 20, "ts": 150}
        ]});
        let sp = spans(&run, 400);
        let names: Vec<&str> = sp.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["invoke_agent aqua", "plan aqua", "chat"]);
        assert!(sp[2]["attributes"].get("gen_ai.request.model").is_none());
        assert_eq!((sp[1]["status"].as_str(), sp[2]["status"].as_str()), (Some("unset"), Some("unset")));
    }

    #[test]
    fn the_steps_are_the_tool_calls_in_order_and_ids_do_not_change_between_asks() {
        let a = trace(&sample(), 5_000);
        let b = trace(&sample(), 9_000);
        let tools: Vec<&str> = a["steps"].as_array().unwrap().iter().map(|s| s["tool"].as_str().unwrap()).collect();
        assert_eq!(tools, vec!["fs.read", "fs.delete"], "what the benchmark's trace grader reads");
        assert_eq!(a["trace_id"], b["trace_id"]);
        assert_eq!(a["trace_id"].as_str().unwrap().len(), 32);
        for i in 0..6 {
            assert_eq!(a["spans"][i]["span_id"], b["spans"][i]["span_id"], "span {i}");
        }
        let ids: std::collections::BTreeSet<&str> = a["spans"].as_array().unwrap().iter().map(|s| s["span_id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 6, "no two spans share an id");
    }

    #[test]
    fn a_run_still_going_ends_now_and_a_step_in_flight_is_unset() {
        let run = json!({"id": "r", "started_at": 100, "ended_at": null, "status": "running", "steps": [
            {"seq": 1, "step": 1, "kind": "tool", "tool": "term.run", "status": "started", "ok": null, "ms": null, "ts": 150}
        ]});
        let sp = spans(&run, 400);
        assert_eq!((sp[0]["end_unix_ms"].as_i64(), sp[0]["status"].as_str()), (Some(400), Some("unset")));
        assert_eq!(sp[1]["status"], "unset");
    }
}
