//! `aqua-cli stats [--days N] [--json]`: what the runs of the last days came to, read from the run database (card OA10-05; task UX-18 of the OpenCode audit).
//!
//! The database is opened **read-only** and is never migrated or written: a statistic must not be able to change what it counts, and the running core may have the file open. A column that an
//! older database does not have (the numbers of the model server arrived with its second migration) makes that line `n/a`, not an error. Nothing of what a run said or did is read: no goal,
//! no arguments, no result, only counts, times and the words `status`, `stopped_reason` and `approval`.

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// One day's questions to the person, by how they came out.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Day {
    /// Questions put to the person (what was answered yes or no, or not at all).
    pub asked: u64,
    pub denied: u64,
    pub timeouts: u64,
    /// Calls that ran on a pre-approval, an answer given earlier in the session, or nothing at all are not questions and are not counted in `asked`.
    pub preapproved: u64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    pub days: u32,
    pub runs: u64,
    /// Runs by `status` (`completed`, `failed`, `cancelled`, ...).
    pub by_status: BTreeMap<String, u64>,
    /// Runs a cap of the autonomy setting stopped.
    pub stopped_by_cap: u64,
    /// Median milliseconds before the first token of a planner reply, and median tokens per second, over the replies the server measured.
    pub median_ttft_ms: Option<f64>,
    pub median_tokens_per_s: Option<f64>,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub per_day: BTreeMap<String, Day>,
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    Some(if n % 2 == 1 { values[n / 2] } else { (values[n / 2 - 1] + values[n / 2]) / 2.0 })
}

/// The runs that began in the last `days` days (counted back from `now_ms`), from the database at `db`.
pub fn collect(db: &Path, days: u32, now_ms: i64) -> Result<Stats, String> {
    if !db.is_file() {
        return Err(format!("there is no run database at {} (no run has been recorded yet, or --data names another folder)", db.display()));
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| format!("the run database could not be opened for reading ({e})"))?;
    let since = now_ms - i64::from(days) * 86_400_000;
    let mut s = Stats { days, ..Default::default() };
    let mut q = conn.prepare("SELECT status, COUNT(*) FROM agent_runs WHERE started_at >= ?1 GROUP BY status").map_err(|e| format!("the run database is not one of Aqua's ({e})"))?;
    for row in q.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map_err(|e| e.to_string())?.flatten() {
        s.runs += row.1 as u64;
        s.by_status.insert(row.0, row.1 as u64);
    }
    // The cap column is newer than the table: a database from before it has none.
    s.stopped_by_cap = conn.query_row("SELECT COUNT(*) FROM agent_runs WHERE started_at >= ?1 AND stopped_by != ''", [since], |r| r.get::<_, i64>(0)).unwrap_or(0) as u64;
    if let Ok(mut q) = conn.prepare("SELECT s.ttft_ms, s.decode_tps_x10 FROM agent_steps s JOIN agent_runs r ON r.id = s.run_id WHERE r.started_at >= ?1 AND s.ttft_ms IS NOT NULL") {
        let rows: Vec<(i64, Option<i64>)> = q.query_map([since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))).map(|m| m.flatten().collect()).unwrap_or_default();
        s.median_ttft_ms = median(rows.iter().map(|r| r.0 as f64).collect());
        s.median_tokens_per_s = median(rows.iter().filter_map(|r| r.1).filter(|t| *t > 0).map(|t| t as f64 / 10.0).collect());
    }
    if let Ok((calls, errors)) = conn.query_row("SELECT COUNT(*), COALESCE(SUM(CASE WHEN s.ok = 0 THEN 1 ELSE 0 END), 0) FROM agent_steps s JOIN agent_runs r ON r.id = s.run_id WHERE s.kind = 'tool' AND s.ok IS NOT NULL AND r.started_at >= ?1", [since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))) {
        (s.tool_calls, s.tool_errors) = (calls as u64, errors as u64);
    }
    if let Ok(mut q) = conn.prepare("SELECT date(ts / 1000, 'unixepoch', 'localtime'), approval, COUNT(*) FROM agent_steps WHERE kind = 'tool' AND approval != '' AND ts >= ?1 GROUP BY 1, 2") {
        for (day, word, n) in q.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))).map(|m| m.flatten().collect::<Vec<_>>()).unwrap_or_default() {
            let d = s.per_day.entry(day).or_default();
            let n = n as u64;
            match word.as_str() {
                // Not questions: an answer given earlier covered the call, or the person allowed the run's checks ahead of time.
                "approved_session" | "auto" => {}
                "preapproved" => d.preapproved += n,
                "denied" => {
                    d.asked += n;
                    d.denied += n;
                }
                "timeout" => {
                    d.asked += n;
                    d.timeouts += n;
                }
                _ => d.asked += n,
            }
        }
    }
    Ok(s)
}

fn one(v: Option<f64>, unit: &str) -> String {
    v.map_or("n/a".to_string(), |v| format!("{v:.1} {unit}"))
}

pub fn render(s: &Stats) -> String {
    let mut out = format!("The last {} day{}: {} run{}\n", s.days, if s.days == 1 { "" } else { "s" }, s.runs, if s.runs == 1 { "" } else { "s" });
    for (status, n) in &s.by_status {
        out.push_str(&format!("  {status:<14} {n}\n"));
    }
    out.push_str(&format!("  stopped by a cap of the autonomy setting: {}\n", s.stopped_by_cap));
    out.push_str(&format!("The model: median time to the first token {}, median speed {}\n", one(s.median_ttft_ms, "ms"), one(s.median_tokens_per_s, "tokens/s")));
    let rate = if s.tool_calls == 0 { "n/a".to_string() } else { format!("{:.1} %", s.tool_errors as f64 * 100.0 / s.tool_calls as f64) };
    out.push_str(&format!("Tool calls: {} ({} failed, error rate {rate})\n", s.tool_calls, s.tool_errors));
    out.push_str("Questions to the person, by day:\n");
    if s.per_day.is_empty() {
        out.push_str("  none\n");
    }
    for (day, d) in &s.per_day {
        out.push_str(&format!("  {day}: asked {}, refused {}, nobody answered {}, ran on a pre-approval {}\n", d.asked, d.denied, d.timeouts, d.preapproved));
    }
    out
}

pub fn to_json(s: &Stats) -> Value {
    json!({
        "days": s.days,
        "runs": s.runs,
        "by_status": s.by_status,
        "stopped_by_cap": s.stopped_by_cap,
        "median_ttft_ms": s.median_ttft_ms,
        "median_tokens_per_s": s.median_tokens_per_s,
        "tool_calls": s.tool_calls,
        "tool_errors": s.tool_errors,
        "approvals_by_day": s.per_day.iter().map(|(d, v)| (d.clone(), json!({ "asked": v.asked, "denied": v.denied, "timeouts": v.timeouts, "preapproved": v.preapproved }))).collect::<serde_json::Map<String, Value>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runs::store::{status, NewRun, NewStep, RunEnd, StepOutcome};
    use crate::agent_runs::AgentRunStore;

    const DAY: i64 = 86_400_000;

    /// A database made by the real store: three runs in the last day (completed, failed with a cap, cancelled), one of 20 days ago, with tool calls and approvals.
    fn fixture(name: &str, now: i64) -> std::path::PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("scratch_cli_tools").join(format!("{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent_runs.db");
        let s = AgentRunStore::open(&path).unwrap();
        let run = |id: &str, at: i64, end_status: &str, stopped_by: &str| {
            s.create_run(&NewRun { id: id.into(), started_at: at, goal_redacted: "SECRET GOAL WORDS".into(), goal_full: "SECRET GOAL WORDS".into(), caller: "api".into(), route: "agent".into(), parent_run_id: None }).unwrap();
            s.end_run(id, at + 1000, &RunEnd { status: end_status.into(), stopped_reason: "final".into(), stopped_by: stopped_by.into(), ..Default::default() }).unwrap();
        };
        let step = |id: &str, n: u32, kind: &str, at: i64, ok: bool, approval: &str, ttft: Option<i64>, tps: Option<i64>| {
            let seq = s.add_step(id, &NewStep { step: n, kind: kind.into(), tool: if kind == "tool" { "term.run".into() } else { String::new() }, args_redacted: "ARGS".into(), idempotency_digest: String::new(), planner_reply: String::new(), ts: at }).unwrap();
            s.finish_step(id, seq, &StepOutcome { ok, status: "completed".into(), approval: approval.into(), ttft_ms: ttft, decode_tps_x10: tps, observation_head: "SECRET RESULT".into(), ..Default::default() }).unwrap();
        };
        run("r1", now - 3_600_000, status::COMPLETED, "");
        run("r2", now - 7_200_000, status::FAILED, "max_tool_calls");
        run("r3", now - 10_800_000, status::CANCELLED, "");
        run("old", now - 20 * DAY, status::COMPLETED, "");
        step("r1", 1, "plan", now - 3_500_000, true, "", Some(1000), Some(80));
        step("r1", 2, "plan", now - 3_400_000, true, "", Some(3000), Some(120));
        step("r1", 3, "tool", now - 3_300_000, true, "approved", None, None);
        step("r1", 4, "tool", now - 3_200_000, false, "denied", None, None);
        step("r2", 1, "tool", now - 7_000_000, true, "preapproved", None, None);
        step("r2", 2, "tool", now - 6_900_000, true, "timeout", None, None);
        step("r2", 3, "tool", now - 6_800_000, true, "approved_session", None, None);
        step("old", 1, "tool", now - 20 * DAY, false, "denied", None, None);
        path
    }

    #[test]
    fn the_numbers_come_from_a_database_the_real_store_made_and_only_the_window_counts() {
        let now = 1_791_000_000_000;
        let db = fixture("window", now);
        let s = collect(&db, 7, now).unwrap();
        assert_eq!(s.runs, 3, "the run of 20 days ago is outside the window");
        assert_eq!((s.by_status["completed"], s.by_status["failed"], s.by_status["cancelled"]), (1, 1, 1));
        assert_eq!(s.stopped_by_cap, 1);
        assert_eq!(s.median_ttft_ms, Some(2000.0), "the median of 1000 and 3000");
        assert_eq!(s.median_tokens_per_s, Some(10.0), "x10 stored: 8.0 and 12.0 tokens/s");
        assert_eq!((s.tool_calls, s.tool_errors), (5, 1));
        let day = s.per_day.values().fold(Day::default(), |mut a, d| {
            a.asked += d.asked;
            a.denied += d.denied;
            a.timeouts += d.timeouts;
            a.preapproved += d.preapproved;
            a
        });
        assert_eq!(day, Day { asked: 3, denied: 1, timeouts: 1, preapproved: 1 }, "approved + denied + timeout were questions; the earlier answer and the pre-approval were not");
        let wide = collect(&db, 30, now).unwrap();
        assert_eq!(wide.runs, 4);
        // Text and JSON say the same, and say nothing of what a run said or did.
        let text = render(&s);
        assert!(text.contains("3 runs") && text.contains("error rate 20.0 %") && text.contains("2000.0 ms"), "{text}");
        assert_eq!(to_json(&s)["runs"], 3);
        for secret in ["SECRET GOAL WORDS", "SECRET RESULT", "ARGS"] {
            assert!(!text.contains(secret) && !to_json(&s).to_string().contains(secret), "{secret}");
        }
    }

    #[test]
    fn a_missing_database_says_so_and_the_file_is_never_written_or_migrated() {
        let now = 1_791_000_000_000;
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("scratch_cli_tools").join("nowhere").join("agent_runs.db");
        assert!(collect(&missing, 7, now).unwrap_err().contains("no run database"));
        let db = fixture("untouched", now);
        let before = std::fs::read(&db).unwrap();
        let _ = collect(&db, 7, now).unwrap();
        assert_eq!(std::fs::read(&db).unwrap(), before, "reading did not change the file");
        // A file that is not one of Aqua's is refused with a sentence, not a panic.
        let other = db.with_file_name("other.db");
        std::fs::write(&other, b"this is not a database").unwrap();
        assert!(collect(&other, 7, now).is_err());
    }

    #[test]
    fn no_runs_is_an_empty_statistic_with_n_a_where_there_is_nothing_to_measure() {
        let now = 1_791_000_000_000;
        let db = fixture("empty", now);
        let s = collect(&db, 7, now + 100 * DAY).unwrap();
        assert_eq!(s.runs, 0);
        let text = render(&s);
        assert!(text.contains("0 runs") && text.contains("n/a") && text.contains("none"), "{text}");
    }
}
