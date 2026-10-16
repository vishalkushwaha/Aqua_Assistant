//! A finished run as OpenTelemetry spans, appended to a file a trace viewer can read (`data/traces/agent_traces_<date>.jsonl`).
//!
//! Off unless `AQUA_TRACE_EXPORT=1`. One line is one run: an OTLP/JSON `resourceSpans` object, so it can be sent to a collector as it is.
//! What a call was given and what it returned are left out unless `AQUA_TRACE_CONTENT=1` (they are redacted either way, but a trace that is
//! sent somewhere should not carry what the person typed). The attribute names are in `otel.rs`, the one place a change of convention is made.

use super::otel as o;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

/// Traces older than this many days are deleted when a new one is written.
pub const KEEP_DAYS: i64 = 14;

pub fn enabled() -> bool {
    matches!(std::env::var("AQUA_TRACE_EXPORT").ok().as_deref().map(str::trim), Some("1" | "on" | "true"))
}

pub fn content_enabled() -> bool {
    matches!(std::env::var("AQUA_TRACE_CONTENT").ok().as_deref().map(str::trim), Some("1" | "on" | "true"))
}

fn attr(key: &str, v: &Value) -> Option<Value> {
    let value = match v {
        Value::String(s) => json!({ "stringValue": s }),
        Value::Bool(b) => json!({ "boolValue": b }),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({ "intValue": n.to_string() }),
        Value::Number(n) => json!({ "doubleValue": n.as_f64()? }),
        _ => return None,
    };
    Some(json!({ "key": key, "value": value }))
}

fn nanos(ms: i64) -> String {
    (ms.max(0) as i128 * 1_000_000).to_string()
}

/// The run as one OTLP/JSON `resourceSpans` document. `now_ms` closes a span that has not ended.
pub fn otlp(run: &Value, now_ms: i64, with_content: bool) -> Value {
    let trace_id = super::trace::trace_id(run["id"].as_str().unwrap_or(""));
    let steps = run["steps"].as_array().cloned().unwrap_or_default();
    let spans: Vec<Value> = super::trace::spans(run, now_ms)
        .into_iter()
        .map(|s| {
            let mut attributes: Vec<Value> = s["attributes"].as_object().map(|m| m.iter().filter_map(|(k, v)| attr(k, v)).collect()).unwrap_or_default();
            if with_content && s["attributes"][o::OPERATION_NAME] == o::OP_EXECUTE_TOOL {
                // The step this span stands for (spans are in step order after the root).
                let name = s["name"].as_str().unwrap_or("");
                if let Some(step) = steps.iter().find(|st| st["kind"] == "tool" && name == format!("execute_tool {}", st["tool"].as_str().unwrap_or(""))) {
                    attributes.extend(attr(o::TOOL_CALL_ARGUMENTS, &Value::String(step["arguments"].to_string())));
                    attributes.extend(attr(o::TOOL_CALL_RESULT, &step["observation"]));
                }
            }
            let code = match s["status"].as_str() {
                Some("ok") => 1,
                Some("error") => 2,
                _ => 0,
            };
            json!({
                "traceId": trace_id,
                "spanId": s["span_id"],
                "parentSpanId": s["parent_span_id"].as_str().unwrap_or(""),
                "name": s["name"],
                // The model call is a client span (3); the run, the planning and the tool calls are internal (1).
                "kind": if s["kind"] == "client" { 3 } else { 1 },
                "startTimeUnixNano": nanos(s["start_unix_ms"].as_i64().unwrap_or(0)),
                "endTimeUnixNano": nanos(s["end_unix_ms"].as_i64().unwrap_or(0)),
                "attributes": attributes,
                "status": { "code": code },
            })
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": { "attributes": [ { "key": "service.name", "value": { "stringValue": "aqua" } } ] },
            "scopeSpans": [{ "scope": { "name": "aqua.agent_runs" }, "spans": spans }],
        }]
    })
}

/// Appends the run, as one line, to today's file in `dir`, and removes files older than [`KEEP_DAYS`].
pub fn append_run(dir: &Path, run: &Value, now_ms: i64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let day = chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_default().format("%Y-%m-%d").to_string();
    let line = otlp(run, now_ms, content_enabled()).to_string();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("agent_traces_{day}.jsonl")))?;
    writeln!(f, "{line}")?;
    remove_old(dir, now_ms);
    Ok(())
}

/// Removes the trace files whose day (in the name) is more than [`KEEP_DAYS`] ago.
fn remove_old(dir: &Path, now_ms: i64) {
    let cutoff = chrono::DateTime::from_timestamp_millis(now_ms - KEEP_DAYS * 24 * 3600 * 1000).unwrap_or_default().date_naive();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let day = name.strip_prefix("agent_traces_").and_then(|n| n.strip_suffix(".jsonl")).and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
        if day.is_some_and(|d| d < cutoff) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Value {
        json!({
            "id": "run_1_1", "started_at": 1_000, "ended_at": 1_900, "status": "completed", "stopped_reason": "final", "caller": "api", "route": "agent", "model": "qwen3-8b",
            "steps": [
                {"seq": 1, "step": 1, "kind": "plan", "tool": "", "status": "completed", "ok": true, "ms": 300, "ts": 1_300, "decision": "", "approval": "", "tokens_in": 812, "tokens_out": 24, "cache_read": 700, "ttft_ms": 310, "decode_tokens_per_second": 3.8},
                {"seq": 2, "step": 1, "kind": "tool", "tool": "fs.read", "arguments": {"path": "notes.txt"}, "observation": "1  Project Aurora", "status": "completed", "ok": true, "ms": 40, "ts": 1_310, "decision": "allowed", "approval": ""},
            ]
        })
    }

    #[test]
    fn a_run_is_an_otlp_document_with_hex_ids_nanosecond_times_and_the_gen_ai_names() {
        let d = otlp(&run(), 5_000, false);
        assert_eq!(d["resourceSpans"][0]["resource"]["attributes"][0], json!({"key": "service.name", "value": {"stringValue": "aqua"}}));
        let spans = d["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 4);
        let root = &spans[0];
        assert_eq!(root["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(root["spanId"].as_str().unwrap().len(), 16);
        assert_eq!((root["parentSpanId"].as_str(), root["startTimeUnixNano"].as_str(), root["endTimeUnixNano"].as_str()), (Some(""), Some("1000000000"), Some("1900000000")));
        assert!(spans.iter().all(|s| s["traceId"] == root["traceId"]), "one trace");
        let (plan, call) = (&spans[1], &spans[2]);
        let has = |s: &Value, key: &str| s["attributes"].as_array().unwrap().iter().find(|a| a["key"] == key).cloned();
        assert_eq!((plan["name"].as_str(), plan["kind"].clone(), call["name"].as_str(), call["kind"].clone()), (Some("plan aqua"), json!(1), Some("chat qwen3-8b"), json!(3)));
        assert_eq!(call["parentSpanId"], plan["spanId"], "the model call is under the plan");
        assert_eq!(has(plan, "gen_ai.operation.name").unwrap()["value"], json!({"stringValue": "plan"}));
        assert_eq!(has(call, "gen_ai.operation.name").unwrap()["value"], json!({"stringValue": "chat"}));
        assert_eq!(has(call, "gen_ai.usage.input_tokens").unwrap()["value"], json!({"intValue": "812"}));
        assert!(has(plan, "gen_ai.usage.input_tokens").is_none(), "the tokens are the call's");
        assert_eq!(has(call, "aqua.usage.decode_tokens_per_second").unwrap()["value"], json!({"doubleValue": 3.8}));
        assert_eq!(root["status"], json!({"code": 1}));
    }

    #[test]
    fn what_a_call_was_given_and_what_it_returned_are_left_out_unless_asked_for() {
        let without = otlp(&run(), 5_000, false).to_string();
        assert!(!without.contains("notes.txt") && !without.contains("Project Aurora"), "{without}");
        let with = otlp(&run(), 5_000, true).to_string();
        assert!(with.contains("notes.txt") && with.contains("Project Aurora"), "{with}");
    }

    #[test]
    fn runs_are_appended_one_to_a_line_and_old_files_go() {
        let dir = std::env::temp_dir().join(format!("aqua_trace_export_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("agent_traces_2000-01-01.jsonl");
        std::fs::write(&old, "{}\n").unwrap();
        let unrelated = dir.join("notes.txt");
        std::fs::write(&unrelated, "keep").unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        append_run(&dir, &run(), now).unwrap();
        append_run(&dir, &run(), now).unwrap();
        let today: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("agent_traces_") && e.path() != old).collect();
        assert_eq!(today.len(), 1);
        let text = std::fs::read_to_string(today[0].path()).unwrap();
        assert_eq!(text.lines().count(), 2, "one line a run");
        assert!(text.lines().all(|l| serde_json::from_str::<Value>(l).is_ok()));
        assert!(!old.exists(), "a file older than {KEEP_DAYS} days is removed");
        assert!(unrelated.exists(), "and nothing else is touched");
    }
}
