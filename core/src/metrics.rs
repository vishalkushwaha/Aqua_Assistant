//! Counters, gauges and histograms kept in memory and read as Prometheus text (`GET /api/v1/metrics`).
//!
//! Only names and numbers go in: the route of a turn, a tool's name, the state a run ended in, seconds and token counts. Never a goal, an
//! argument, an answer or a file name, so the page can be read (and scraped) without it telling anyone what was asked. Nothing is
//! written to disk; the numbers start again when Aqua does.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// Upper edges of the histogram buckets, in seconds. The last bucket is "more than this" (`+Inf`).
const BUCKETS: [f64; 12] = [0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

#[derive(Default, Clone)]
struct Histogram {
    /// `counts[i]` is the number of observations at or below `BUCKETS[i]` and above the previous edge; the last is everything above.
    counts: [u64; 13],
    sum: f64,
    count: u64,
}

#[derive(Default)]
struct Registry {
    counters: BTreeMap<(String, String), f64>,
    gauges: BTreeMap<(String, String), f64>,
    histograms: BTreeMap<(String, String), Histogram>,
    help: BTreeMap<String, &'static str>,
}

fn registry() -> MutexGuard<'static, Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Registry::default())).lock().unwrap_or_else(PoisonError::into_inner)
}

/// A label value made safe for the text format: letters, digits and `_ . : / -` stay, anything else (a quote, a line break, a space) becomes `_`.
fn safe(value: &str) -> String {
    value.chars().take(64).map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '/' | '-') { c } else { '_' }).collect()
}

fn labels_text(labels: &[(&str, &str)]) -> String {
    labels.iter().map(|(k, v)| format!("{k}=\"{}\"", safe(v))).collect::<Vec<_>>().join(",")
}

/// Says what a metric is, once (shown as its `# HELP` line).
pub fn describe(name: &str, help: &'static str) {
    registry().help.entry(name.to_string()).or_insert(help);
}

/// Adds 1 to a counter.
pub fn inc(name: &str, labels: &[(&str, &str)]) {
    add(name, labels, 1.0);
}

pub fn add(name: &str, labels: &[(&str, &str)], by: f64) {
    *registry().counters.entry((name.to_string(), labels_text(labels))).or_insert(0.0) += by;
}

/// Sets a gauge (a value that goes up and down: the last decode speed, whether the audit chain verified).
pub fn gauge(name: &str, labels: &[(&str, &str)], value: f64) {
    registry().gauges.insert((name.to_string(), labels_text(labels)), value);
}

/// Records how long something took, in seconds.
pub fn observe_seconds(name: &str, labels: &[(&str, &str)], seconds: f64) {
    if !seconds.is_finite() || seconds < 0.0 {
        return;
    }
    let mut r = registry();
    let h = r.histograms.entry((name.to_string(), labels_text(labels))).or_default();
    let at = BUCKETS.iter().position(|edge| seconds <= *edge).unwrap_or(BUCKETS.len());
    h.counts[at] += 1;
    h.sum += seconds;
    h.count += 1;
}

pub fn counter_value(name: &str, labels: &[(&str, &str)]) -> f64 {
    registry().counters.get(&(name.to_string(), labels_text(labels))).copied().unwrap_or(0.0)
}

/// The sum of a counter over all its labels.
pub fn counter_total(name: &str) -> f64 {
    registry().counters.iter().filter(|((n, _), _)| n == name).map(|(_, v)| *v).sum()
}

/// A quantile (0 to 1) of a histogram, by linear interpolation inside the bucket it falls in. `None` before any observation.
pub fn quantile(name: &str, labels: &[(&str, &str)], q: f64) -> Option<f64> {
    let r = registry();
    let h = r.histograms.get(&(name.to_string(), labels_text(labels)))?;
    if h.count == 0 {
        return None;
    }
    let target = (q.clamp(0.0, 1.0) * h.count as f64).max(1.0);
    let mut seen = 0.0;
    for (i, c) in h.counts.iter().enumerate() {
        let c = *c as f64;
        if seen + c >= target {
            let lower = if i == 0 { 0.0 } else { BUCKETS[i - 1] };
            let upper = BUCKETS.get(i).copied().unwrap_or(BUCKETS[BUCKETS.len() - 1] * 2.0);
            let within = if c > 0.0 { (target - seen) / c } else { 0.0 };
            return Some(lower + (upper - lower) * within);
        }
        seen += c;
    }
    None
}

fn line(name: &str, labels: &str, value: f64) -> String {
    let v = if value.fract() == 0.0 && value.abs() < 1e15 { format!("{}", value as i64) } else { format!("{value}") };
    if labels.is_empty() {
        format!("{name} {v}\n")
    } else {
        format!("{name}{{{labels}}} {v}\n")
    }
}

/// What each of Aqua's own metrics is.
fn describe_all() {
    for (name, help) in [
        ("aqua_turns_total", "Turns handled, by how they were answered (chat, agent, local, weather, follow_up)."),
        ("aqua_turn_seconds", "Seconds a turn took, by how it was answered."),
        ("aqua_agent_runs_total", "Agent runs, by the state they ended in."),
        ("aqua_planner_replies_total", "Replies of the planning model."),
        ("aqua_planner_seconds", "Seconds the planning model took for one reply."),
        ("aqua_planner_tokens_total", "Planner tokens: prompt_read (read by the server), prompt_cached (taken from its cache), generated."),
        ("aqua_model_calls_total", "Replies of the planning model, by the model that wrote them and the role it was asked in (agent). The same replies as aqua_planner_replies_total."),
        ("aqua_model_tokens_total", "Planner tokens as the model server measured them, by model and role: prompt_read (read by the server), prompt_cached (taken from its cache), generated. The same numbers as aqua_planner_tokens_total."),
        ("aqua_planner_decode_tokens_per_second", "How fast the planning model wrote its last reply."),
        ("aqua_tool_calls_total", "Tool calls made by the agent, by tool and what was decided (allowed, denied, error)."),
        ("aqua_tool_seconds", "Seconds a tool call took."),
        ("aqua_approvals_total", "Questions put to the person about a tool call, by how they were answered."),
        ("aqua_model_loads_total", "Times the model server was started (each costs a load of the model)."),
        ("aqua_errors_total", "Errors Aqua handled, by kind."),
        ("aqua_context_overflow_total", "Prompts the model server refused as too long for its window, by whether asking again with less of the earlier results got an answer (recovered)."),
        ("aqua_model_retries_total", "Times the model router asked a model service again after it said later (rate limited or overloaded), by the class of the error."),
        ("aqua_model_shrinks_total", "Times a caller was asked to make a turn shorter after the model refused it as too long, by whether the shorter turn was answered (recovered)."),
        ("aqua_model_idle_timeouts_total", "Streamed answers of the local model that were hung up on because nothing came for too long, by whether it was before the first piece or in the answer."),
        ("aqua_model_egress_refused_total", "Model calls the egress gate refused before they were made (a model on another machine), by where the model is."),
        ("aqua_one_door_total", "Browser and desktop calls, by what the tool host would have decided and what the old route did (the one-door comparison)."),
    ] {
        describe(name, help);
    }
}

/// Everything, in the Prometheus text format (version 0.0.4).
pub fn render() -> String {
    describe_all();
    let r = registry();
    let mut out = String::new();
    let mut declared: Vec<String> = Vec::new();
    let mut header = |out: &mut String, name: &str, kind: &str| {
        if !declared.iter().any(|n| n == name) {
            declared.push(name.to_string());
            if let Some(h) = r.help.get(name) {
                out.push_str(&format!("# HELP {name} {h}\n"));
            }
            out.push_str(&format!("# TYPE {name} {kind}\n"));
        }
    };
    for ((name, labels), v) in &r.counters {
        header(&mut out, name, "counter");
        out.push_str(&line(name, labels, *v));
    }
    for ((name, labels), v) in &r.gauges {
        header(&mut out, name, "gauge");
        out.push_str(&line(name, labels, *v));
    }
    for ((name, labels), h) in &r.histograms {
        header(&mut out, name, "histogram");
        let mut cumulative = 0u64;
        for (i, edge) in BUCKETS.iter().enumerate() {
            cumulative += h.counts[i];
            let le = format!("le=\"{edge}\"");
            let l = if labels.is_empty() { le } else { format!("{labels},{le}") };
            out.push_str(&line(&format!("{name}_bucket"), &l, cumulative as f64));
        }
        let l = if labels.is_empty() { "le=\"+Inf\"".to_string() } else { format!("{labels},le=\"+Inf\"") };
        out.push_str(&line(&format!("{name}_bucket"), &l, h.count as f64));
        out.push_str(&line(&format!("{name}_sum"), labels, h.sum));
        out.push_str(&line(&format!("{name}_count"), labels, h.count as f64));
    }
    out
}

/// The few numbers a diagnostics page shows live: how long planning takes, how much of each prompt the server already had, how many tool
/// calls were refused or asked about.
pub fn summary() -> Value {
    let read = counter_value("aqua_planner_tokens_total", &[("kind", "prompt_read")]);
    let cached = counter_value("aqua_planner_tokens_total", &[("kind", "prompt_cached")]);
    let ms = |q| quantile("aqua_planner_seconds", &[], q).map(|s| (s * 1000.0).round());
    json!({
        "planner_ms_p50": ms(0.5),
        "planner_ms_p90": ms(0.9),
        "prompt_cache_hit_rate": if read + cached > 0.0 { Some((cached / (read + cached) * 1000.0).round() / 1000.0) } else { None },
        "planner_replies": counter_total("aqua_planner_replies_total"),
        "tool_calls": counter_total("aqua_tool_calls_total"),
        "tool_calls_refused": counter_value("aqua_tool_calls_total", &[("decision", "denied")]),
        "approvals_asked": counter_total("aqua_approvals_total"),
        "agent_runs": counter_total("aqua_agent_runs_total"),
        "turns": counter_total("aqua_turns_total"),
        "model_loads": counter_total("aqua_model_loads_total"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_add_up_per_label_and_render_as_prometheus_text() {
        inc("t_calls_total", &[("tool", "fs.read"), ("decision", "allowed")]);
        inc("t_calls_total", &[("tool", "fs.read"), ("decision", "allowed")]);
        add("t_calls_total", &[("tool", "fs.delete"), ("decision", "denied")], 3.0);
        describe("t_calls_total", "Tool calls by tool and what was decided.");
        assert_eq!(counter_value("t_calls_total", &[("tool", "fs.read"), ("decision", "allowed")]), 2.0);
        assert_eq!(counter_total("t_calls_total"), 5.0);
        let text = render();
        assert!(text.contains("# HELP t_calls_total Tool calls by tool and what was decided.\n# TYPE t_calls_total counter\n"), "{text}");
        assert!(text.contains("t_calls_total{tool=\"fs.read\",decision=\"allowed\"} 2\n"), "{text}");
        assert!(text.contains("t_calls_total{tool=\"fs.delete\",decision=\"denied\"} 3\n"), "{text}");
        assert_eq!(text.matches("# TYPE t_calls_total").count(), 1, "declared once");
    }

    #[test]
    fn a_label_cannot_break_the_text_format_or_carry_content() {
        inc("t_labels_total", &[("tool", "x\"} 1\nevil_metric 99 {y=\"")]);
        let text = render();
        assert!(!text.contains("evil_metric 99") && !text.lines().any(|l| l.starts_with("evil_metric")), "{text}");
        assert!(text.contains("t_labels_total{tool=\"x___1_evil_metric_99__y__\"} 1"), "{text}");
        inc("t_long_total", &[("tool", &"a".repeat(500))]);
        assert!(render().lines().any(|l| l.starts_with("t_long_total{tool=\"") && l.len() < 100), "cut to 64 characters");
    }

    #[test]
    fn a_histogram_counts_cumulatively_and_gives_quantiles() {
        for s in [0.02, 0.02, 0.2, 0.2, 0.2, 3.0, 3.0, 40.0, 200.0, 0.4] {
            observe_seconds("t_seconds", &[("route", "chat")], s);
        }
        observe_seconds("t_seconds", &[("route", "chat")], f64::NAN);
        observe_seconds("t_seconds", &[("route", "chat")], -1.0);
        let text = render();
        assert!(text.contains("t_seconds_bucket{route=\"chat\",le=\"0.05\"} 2\n"), "{text}");
        assert!(text.contains("t_seconds_bucket{route=\"chat\",le=\"0.25\"} 5\n"), "{text}");
        assert!(text.contains("t_seconds_bucket{route=\"chat\",le=\"+Inf\"} 10\n"), "{text}");
        assert!(text.contains("t_seconds_count{route=\"chat\"} 10\n"), "nan and negative are not observations");
        let p50 = quantile("t_seconds", &[("route", "chat")], 0.5).unwrap();
        assert!((0.1..=0.5).contains(&p50), "the middle of ten is in the 0.1 to 0.5 buckets: {p50}");
        let p90 = quantile("t_seconds", &[("route", "chat")], 0.9).unwrap();
        assert!(p90 > p50 && p90 > 10.0, "{p90}");
        assert_eq!(quantile("t_seconds", &[("route", "none")], 0.5), None);
    }

    #[test]
    fn gauges_hold_the_last_value_and_the_summary_reads_the_counters() {
        gauge("t_decode_tps", &[], 3.8);
        gauge("t_decode_tps", &[], 4.1);
        assert!(render().contains("t_decode_tps 4.1\n"));
        add("aqua_planner_tokens_total", &[("kind", "prompt_read")], 100.0);
        add("aqua_planner_tokens_total", &[("kind", "prompt_cached")], 300.0);
        let s = summary();
        let rate = s["prompt_cache_hit_rate"].as_f64().unwrap();
        assert!((0.0..=1.0).contains(&rate), "{s}");
    }
}
