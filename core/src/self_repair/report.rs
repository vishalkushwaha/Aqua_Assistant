//! "What Aqua did by itself this week" (card R7-20): one report from the tool host's audit log and the proposals table, so the person can see the loop working and catch it going wrong.
//!
//! It says what Aqua noticed (gaps), looked up (web and research calls of agent runs), proposed, what was applied with an approval (and who gave it), and what was undone. Changes that
//! have no undo are listed on their own, each marked `no_undo: true`, so a screen can show them in red. Everything comes from records that already exist: the log is read, never
//! written, and its hash chain is checked once, by the function the caller passes in (or not at all). A line in the log that cannot be read is skipped.

use super::store::{Gap, Proposal, RepairStore, R};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

pub const DEFAULT_DAYS: u32 = 7;
pub const MAX_DAYS: u32 = 90;

/// Items listed in one section (counts are always complete).
const MAX_ITEMS: usize = 100;
/// Groups of changes kept while reading the log.
const MAX_GROUPS: usize = 2000;
/// How many rotated files the ledger keeps next to the live one (`AuditLedger`'s own number).
const KEEP_ROTATED: usize = 5;

/// The classes (as the ledger writes them) of a call that changes something.
const CHANGING: [&str; 5] = ["WRITE", "EXECUTE", "DESTRUCTIVE", "PRIVILEGED", "EXTERNALEFFECT"];
/// The file tools whose changes `fs.undo_run` puts back, when the undo store still holds them.
const UNDOABLE_BY_RUN: [&str; 7] = ["fs.write", "fs.edit", "fs.edit_lines", "fs.patch", "fs.copy", "fs.move", "fs.delete"];
/// The tools that are themselves undoing something: not a change to list.
const UNDOING: [&str; 2] = ["fs.restore", "fs.undo_run"];
const LOOKUP_PREFIXES: [&str; 2] = ["web.", "research."];
const TARGET_KEYS: [&str; 6] = ["path", "src", "folder", "url", "command", "name"];

#[derive(Deserialize)]
struct Approval {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    by: String,
}

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    ts_unix_ms: u64,
    #[serde(default)]
    request_id: String,
    #[serde(default)]
    agent_run_id: String,
    #[serde(default)]
    caller: String,
    #[serde(default)]
    tool: String,
    #[serde(default)]
    args: Value,
    #[serde(default)]
    decision: String,
    #[serde(default)]
    classes: Vec<String>,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    approval: Option<Approval>,
}

struct ChangeGroup {
    calls: u64,
    first_ms: u64,
    last_ms: u64,
    targets: Vec<String>,
}

#[derive(Default)]
struct Audit {
    files_read: usize,
    lines_in_window: u64,
    oldest_ms: Option<u64>,
    first_seq: Option<u64>,
    /// Tool -> (succeeded, failed).
    lookups: BTreeMap<String, (u64, u64)>,
    lookup_runs: BTreeSet<String>,
    /// (run, tool) -> the successful state-changing calls of agent runs.
    changes: BTreeMap<(String, String), ChangeGroup>,
    changes_dropped: u64,
    /// `fs.allow_folder` calls: request id -> (kind of answer, who gave it).
    grants: BTreeMap<String, (String, String)>,
}

/// The ledger's files, oldest first: the rotated ones, then the live one.
fn audit_files(live: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = (1..=KEEP_ROTATED).rev().map(|n| live.with_extension(format!("{n}.jsonl"))).filter(|p| p.exists()).collect();
    files.push(live.to_path_buf());
    files
}

/// Aqua did this by itself: a run of the agent or a replayed recipe, not a click, a request of a script or a spoken command.
fn by_itself(l: &Line) -> bool {
    l.caller == "agent" || l.caller == "replay"
}

fn target_of(args: &Value) -> Option<String> {
    TARGET_KEYS.iter().find_map(|k| args.get(*k).and_then(|v| v.as_str())).map(|t| super::store::clean_text(t, 120))
}

fn read_audit(live: &Path, since_ms: u64, until_ms: u64) -> Audit {
    let mut a = Audit::default();
    for file in audit_files(live) {
        let Ok(f) = std::fs::File::open(&file) else { continue };
        a.files_read += 1;
        for raw in std::io::BufReader::new(f).split(b'\n').map_while(Result::ok) {
            let Ok(l) = serde_json::from_slice::<Line>(&raw) else { continue };
            // Each call writes a line when it starts and another when it ends; the second says how it went. A read that failed for a moment and was made once more (card OA5-04,
            // TL-15) has a line for the try that failed (`:retry`) before the line that ends it: it is not what the call came to.
            if l.error_code.as_deref() == Some("STARTED") || l.decision.ends_with(":started") || l.decision.ends_with(":retry") {
                continue;
            }
            a.oldest_ms = Some(a.oldest_ms.map_or(l.ts_unix_ms, |o| o.min(l.ts_unix_ms)));
            a.first_seq = Some(a.first_seq.map_or(l.seq, |s| s.min(l.seq)));
            if l.ts_unix_ms < since_ms || l.ts_unix_ms > until_ms {
                continue;
            }
            a.lines_in_window += 1;
            if l.tool == "fs.allow_folder" {
                if let Some(ap) = &l.approval {
                    a.grants.insert(l.request_id.clone(), (ap.kind.clone(), ap.by.clone()));
                }
            }
            if !by_itself(&l) {
                continue;
            }
            if LOOKUP_PREFIXES.iter().any(|p| l.tool.starts_with(p)) {
                let e = a.lookups.entry(l.tool.clone()).or_default();
                if l.ok {
                    e.0 += 1;
                } else {
                    e.1 += 1;
                }
                if !l.agent_run_id.is_empty() {
                    a.lookup_runs.insert(l.agent_run_id.clone());
                }
            }
            if l.ok && l.classes.iter().any(|c| CHANGING.contains(&c.as_str())) && !UNDOING.contains(&l.tool.as_str()) {
                let key = (l.agent_run_id.clone(), l.tool.clone());
                if !a.changes.contains_key(&key) && a.changes.len() >= MAX_GROUPS {
                    a.changes_dropped += 1;
                    continue;
                }
                let g = a.changes.entry(key).or_insert(ChangeGroup { calls: 0, first_ms: l.ts_unix_ms, last_ms: l.ts_unix_ms, targets: Vec::new() });
                g.calls += 1;
                g.first_ms = g.first_ms.min(l.ts_unix_ms);
                g.last_ms = g.last_ms.max(l.ts_unix_ms);
                if let Some(t) = target_of(&l.args) {
                    if g.targets.len() < 3 && !g.targets.contains(&t) {
                        g.targets.push(t);
                    }
                }
            }
        }
    }
    a
}

/// Whether a change can be taken back, and if not, why not (in the words the report uses).
fn undo_state(tool: &str, run: &str, has_undo: &dyn Fn(&str) -> bool, folders_can_be_taken_back: bool) -> (bool, &'static str) {
    if UNDOABLE_BY_RUN.contains(&tool) {
        if !run.is_empty() && has_undo(run) {
            (true, "")
        } else {
            (false, "the undo store holds nothing for this run, so fs.undo_run cannot restore it")
        }
    } else if tool == "fs.allow_folder" {
        // With `fs.revoke_folder` a grant has an undo: it is taken back by its name or its place, and it ends when Aqua restarts in any case.
        if folders_can_be_taken_back {
            (true, "")
        } else {
            (false, "a folder grant lasts until Aqua restarts and cannot be taken back sooner")
        }
    } else if tool == "fs.set_attrs" {
        (false, "the undo store does not record attribute changes")
    } else {
        (false, "this kind of tool has no undo")
    }
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

fn counts_by<'a>(items: impl Iterator<Item = &'a str>) -> Map<String, Value> {
    let mut m: BTreeMap<String, u64> = BTreeMap::new();
    for i in items {
        *m.entry(i.to_string()).or_default() += 1;
    }
    m.into_iter().map(|(k, v)| (k, json!(v))).collect()
}

fn count_text(m: &Map<String, Value>) -> String {
    m.iter().map(|(k, v)| format!("{} {}", v, k.replace('_', " "))).collect::<Vec<_>>().join(", ")
}

/// What the report is made from.
pub struct ReportInput<'a> {
    /// The tool host's ledger (`data/tools/host.audit.jsonl`); its rotated files are read too. A file that is not there is an empty log.
    pub audit_path: &'a Path,
    pub days: u32,
    /// Now, in seconds.
    pub now: i64,
    /// Does the undo store of the file tools hold changes of this agent run (so `fs.undo_run` can restore them)?
    pub has_undo: &'a dyn Fn(&str) -> bool,
    /// Can the tool host take a folder grant back before Aqua restarts? (The running host can, through `fs.revoke_folder`; the route asks it.)
    pub folders_can_be_taken_back: bool,
    /// Checks the hash chain of the whole ledger and gives the number of lines it checked. Called at most once; `None` skips the check.
    pub verify_chain: Option<&'a dyn Fn() -> Result<u64, String>>,
}

fn proposal_row(p: &Proposal, grants: &BTreeMap<String, (String, String)>) -> Value {
    let host_asked = p.result["request_id"].as_str().and_then(|id| grants.get(id)).map(|(kind, by)| json!({ "kind": kind, "by": by }));
    json!({
        "id": p.id,
        "gap_id": p.gap_id,
        "kind": p.kind,
        "title": p.title,
        "status": p.status,
        "made_at": p.at,
        "approved_by": p.approved_by,
        "approved_at": p.approved_at,
        "applied_at": p.applied_at,
        "undone_at": p.undone_at,
        "host_asked": host_asked,
        "error": p.error,
    })
}

fn gap_row(g: &Gap, since: i64) -> Value {
    json!({ "id": g.id, "kind": g.kind, "summary": g.summary, "status": g.status, "count": g.count, "first_at": g.at, "last_at": g.last_at, "again": g.at < since })
}

/// Build the report for the last `input.days` days: a JSON body with a plain-text version of it in `text`.
pub fn build(store: &RepairStore, input: ReportInput<'_>) -> R<Value> {
    let days = input.days.clamp(1, MAX_DAYS);
    let (until, since) = (input.now, input.now - i64::from(days) * 86_400);
    let audit = read_audit(input.audit_path, (since.max(0) as u64) * 1000, (until.max(0) as u64) * 1000 + 999);

    // What was noticed.
    let gaps = store.gaps_since(since)?;
    let new_gaps = gaps.iter().filter(|g| g.at >= since).count();
    let again_gaps = gaps.len() - new_gaps;
    let gaps_by_kind = counts_by(gaps.iter().map(|g| g.kind.as_str()));

    // What was looked up.
    let lookup_calls: u64 = audit.lookups.values().map(|(ok, failed)| ok + failed).sum();
    let lookup_failed: u64 = audit.lookups.values().map(|(_, failed)| *failed).sum();
    let lookups_by_tool: Map<String, Value> = audit.lookups.iter().map(|(t, (ok, failed))| (t.clone(), json!({ "ok": ok, "failed": failed }))).collect();

    // What was proposed, applied and undone.
    let proposals = store.proposals_since(since)?;
    let made: Vec<&Proposal> = proposals.iter().filter(|p| p.at >= since).collect();
    let applied: Vec<&Proposal> = proposals.iter().filter(|p| p.applied_at.is_some_and(|t| t >= since)).collect();
    let undone: Vec<&Proposal> = proposals.iter().filter(|p| p.undone_at.is_some_and(|t| t >= since)).collect();
    let rejected = made.iter().filter(|p| p.status == "rejected").count();
    let failed = proposals.iter().filter(|p| p.status == "failed" && p.approved_at.is_some_and(|t| t >= since)).count();

    // Changes with no undo: calls of agent runs that nothing can put back, and applied proposals with no way back.
    let mut no_undo: Vec<Value> = Vec::new();
    let (mut no_undo_calls, mut undoable_calls) = (0u64, 0u64);
    for ((run, tool), g) in &audit.changes {
        let (undoable, why) = undo_state(tool, run, input.has_undo, input.folders_can_be_taken_back);
        if undoable {
            undoable_calls += g.calls;
            continue;
        }
        no_undo_calls += g.calls;
        no_undo.push(json!({ "no_undo": true, "source": "tool_call", "tool": tool, "run_id": run, "calls": g.calls, "first_at": g.first_ms / 1000, "last_at": g.last_ms / 1000, "targets": g.targets, "why": why }));
    }
    for p in applied.iter().filter(|p| p.status == "applied") {
        let kind = p.undo["kind"].as_str().unwrap_or("");
        let back = match kind {
            "revoke_folder" => input.folders_can_be_taken_back,
            "flow_version" | "nothing" | "delete_recipe_draft" | "draft_already_kept" => true,
            _ => false,
        };
        if !back {
            no_undo_calls += 1;
            let why = if kind == "revoke_folder" { "a folder grant lasts until Aqua restarts and the tool host cannot take it back sooner" } else { "this change has no recorded undo" };
            no_undo.push(json!({ "no_undo": true, "source": "proposal", "proposal_id": p.id, "tool": p.kind, "title": p.title, "applied_at": p.applied_at, "approved_by": p.approved_by, "why": why }));
        }
    }
    let no_undo_total = no_undo.len();
    no_undo.truncate(MAX_ITEMS);

    // The log itself: how much of the window it covers, and whether its chain holds.
    let rotated_away = audit.first_seq.is_some_and(|s| s > 1);
    let covers_window = !rotated_away || audit.oldest_ms.is_some_and(|o| (o / 1000) as i64 <= since);
    let chain = match input.verify_chain {
        Some(check) => match check() {
            Ok(lines) => json!({ "checked": true, "ok": true, "lines": lines }),
            Err(problem) => json!({ "checked": true, "ok": false, "problem": super::store::clean_text(&problem, 300) }),
        },
        None => json!({ "checked": false }),
    };

    // The plain-text version.
    let mut text: Vec<String> = Vec::new();
    text.push(format!("What Aqua did by itself: the last {days} days, up to {}", fmt_ts(until)));
    if gaps.is_empty() {
        text.push("Noticed: nothing that it could not do.".into());
    } else {
        text.push(format!("Noticed: {} thing{} it could not do ({}); {again_gaps} had been noticed before and happened again.", gaps.len(), if gaps.len() == 1 { "" } else { "s" }, count_text(&gaps_by_kind)));
    }
    if lookup_calls == 0 {
        text.push("Looked up: nothing.".into());
    } else {
        let by_tool = audit.lookups.iter().map(|(t, (ok, failed))| format!("{t} {}", ok + failed)).collect::<Vec<_>>().join(", ");
        text.push(format!("Looked up: {lookup_calls} web or research call{} in {} run{} ({by_tool}); {lookup_failed} failed.", if lookup_calls == 1 { "" } else { "s" }, audit.lookup_runs.len(), if audit.lookup_runs.len() == 1 { "" } else { "s" }));
    }
    text.push(format!("Proposed: {} fix{}. Applied with your approval: {}. Undone: {}. Rejected: {rejected}. Not applied: {failed}.", made.len(), if made.len() == 1 { "" } else { "es" }, applied.len(), undone.len()));
    for p in &applied {
        let host = p.result["request_id"].as_str().and_then(|id| audit.grants.get(id)).map(|(kind, by)| format!("; the tool host asked again and the answer was {kind} (by {by})")).unwrap_or_default();
        text.push(format!("  applied #{} \"{}\" at {}, approved by {}{host}", p.id, p.title, p.applied_at.map(fmt_ts).unwrap_or_default(), if p.approved_by.is_empty() { "nobody recorded" } else { p.approved_by.as_str() }));
    }
    for p in &undone {
        text.push(format!("  undone #{} \"{}\" at {}", p.id, p.title, p.undone_at.map(fmt_ts).unwrap_or_default()));
    }
    if no_undo_total == 0 {
        text.push(format!("Changes with no undo: none ({undoable_calls} change{} by agent runs can be put back).", if undoable_calls == 1 { "" } else { "s" }));
    } else {
        text.push(format!("CHANGES WITH NO UNDO ({no_undo_calls}):"));
        for item in &no_undo {
            let what = item["tool"].as_str().unwrap_or("");
            let who = item["run_id"].as_str().filter(|r| !r.is_empty()).map(|r| format!(" in run {r}")).unwrap_or_default();
            let n = item["calls"].as_u64().map(|c| format!(" ({c} call{})", if c == 1 { "" } else { "s" })).unwrap_or_default();
            text.push(format!("  [NO UNDO] {what}{who}{n}: {}", item["why"].as_str().unwrap_or("")));
        }
    }
    text.push(if !covers_window { format!("The audit log only goes back to {}; older calls were rotated away.", audit.oldest_ms.map(|o| fmt_ts((o / 1000) as i64)).unwrap_or_default()) } else { format!("The audit log covers the whole window ({} lines in it).", audit.lines_in_window) });
    match chain["ok"].as_bool() {
        Some(true) => text.push("The audit log's chain was checked and holds.".into()),
        Some(false) => text.push(format!("WARNING: the audit log's chain did not check out: {}", chain["problem"].as_str().unwrap_or(""))),
        None => text.push("The audit log's chain was not checked for this report.".into()),
    }

    Ok(json!({
        "window": { "days": days, "since": since, "until": until },
        "noticed": {
            "total": gaps.len(),
            "new": new_gaps,
            "again": again_gaps,
            "by_kind": gaps_by_kind,
            "gaps": gaps.iter().take(MAX_ITEMS).map(|g| gap_row(g, since)).collect::<Vec<_>>(),
        },
        "looked_up": { "calls": lookup_calls, "failed": lookup_failed, "runs": audit.lookup_runs.len(), "by_tool": lookups_by_tool },
        "proposed": {
            "total": made.len(),
            "by_kind": counts_by(made.iter().map(|p| p.kind.as_str())),
            "by_status": counts_by(made.iter().map(|p| p.status.as_str())),
            "items": made.iter().take(MAX_ITEMS).map(|p| proposal_row(p, &audit.grants)).collect::<Vec<_>>(),
        },
        "applied": applied.iter().take(MAX_ITEMS).map(|p| proposal_row(p, &audit.grants)).collect::<Vec<_>>(),
        "undone": undone.iter().take(MAX_ITEMS).map(|p| proposal_row(p, &audit.grants)).collect::<Vec<_>>(),
        "rejected": rejected,
        "not_applied": failed,
        "changes_without_undo": { "no_undo": true, "count": no_undo_calls, "items": no_undo, "items_total": no_undo_total, "not_listed": audit.changes_dropped },
        "changes_with_undo": { "calls": undoable_calls },
        "ledger": {
            "files_read": audit.files_read,
            "lines_in_window": audit.lines_in_window,
            "oldest_line_at": audit.oldest_ms.map(|o| o / 1000),
            "covers_window": covers_window,
            "chain": chain,
        },
        "text": text.join("\n"),
    }))
}

#[cfg(test)]
mod tests {
    use super::super::store::{GapKind, NewGap, NewProposal};
    use super::super::test_support::temp_dir;
    use super::*;

    const NOW: i64 = 2_000_000_000;
    const DAY: i64 = 86_400;

    /// One line of the ledger as the host writes it (the hashes are not needed by the report).
    #[allow(clippy::too_many_arguments)]
    fn line(seq: u64, at: i64, caller: &str, tool: &str, run: &str, ok: bool, classes: &[&str], args: Value, request_id: &str, approval: Option<(&str, &str)>) -> String {
        json!({
            "seq": seq, "ts_unix_ms": at * 1000, "prev": "p", "request_id": request_id, "session_id": "ses_local", "agent_run_id": run, "caller": caller, "tool": tool, "args": args,
            "decision": "auto", "classes": classes, "ok": ok, "error_code": null, "duration_ms": 3,
            "approval": approval.map(|(kind, by)| json!({ "kind": kind, "by": by, "reason": "r" })), "hash": "h",
        })
        .to_string()
    }

    fn write_log(dir: &Path) -> PathBuf {
        let in_window = NOW - 2 * DAY;
        let started = json!({ "seq": 99, "ts_unix_ms": in_window * 1000, "agent_run_id": "run_c", "caller": "agent", "tool": "fs.write", "decision": "auto:started", "classes": ["WRITE"], "ok": false, "error_code": "STARTED" }).to_string();
        let lines = [
            line(1, in_window, "agent", "web.search", "run_a", true, &["NETWORK"], json!({ "query": "q" }), "r1", None),
            line(2, in_window, "agent", "web.search", "run_a", true, &["NETWORK"], json!({ "query": "q2" }), "r2", None),
            line(3, in_window, "agent", "web.read", "run_a", false, &["NETWORK"], json!({ "url": "https://example.com/a" }), "r3", None),
            line(4, in_window, "agent", "research.run", "run_b", true, &["NETWORK"], json!({ "question": "q" }), "r4", None),
            line(5, in_window, "agent", "fs.write", "run_a", true, &["WRITE"], json!({ "path": "E:/work/a.txt" }), "r5", None),
            line(6, in_window, "agent", "fs.write", "run_c", true, &["WRITE"], json!({ "path": "E:/work/b.txt" }), "r6", None),
            line(7, in_window, "agent", "term.run", "run_c", true, &["EXECUTE"], json!({ "command": "dir" }), "r7", None),
            line(8, in_window, "agent", "fs.read", "run_c", true, &["READ"], json!({ "path": "E:/work/b.txt" }), "r8", None),
            line(9, in_window, "ui", "fs.write", "", true, &["WRITE"], json!({ "path": "E:/work/by_hand.txt" }), "r9", None),
            line(10, in_window, "ui", "fs.allow_folder", "", true, &["PRIVILEGED"], json!({ "path": "E:/work" }), "req_allow", Some(("once", "ui"))),
            line(11, NOW - 20 * DAY, "agent", "web.search", "run_old", true, &["NETWORK"], json!({ "query": "old" }), "r11", None),
            started,
            line(13, in_window, "agent", "fs.write", "run_d", false, &["WRITE"], json!({ "path": "E:/work/refused.txt" }), "r13", None),
            "this line is not json".to_string(),
        ];
        let path = dir.join("host.audit.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn the_report_counts_what_was_noticed_looked_up_proposed_applied_and_undone_and_lists_what_has_no_undo() {
        let dir = temp_dir("report");
        let log = write_log(&dir);
        let store = RepairStore::new(dir.join("self_repair.db"));
        let in_window = NOW - 2 * DAY;
        // Two gaps: one first noticed in the window, one noticed before it that happened again inside it.
        let mut fresh = NewGap::new(GapKind::PlatformNotBuilt, "1:publish:facebook", "Posting to Facebook is not built yet");
        fresh.flow_id = Some(1);
        let fresh = store.record(fresh, in_window).unwrap();
        let old = store.record(NewGap::new(GapKind::FolderNotAllowed, "e:/work", "The folder E:/work is outside the folders Aqua may use"), NOW - 30 * DAY).unwrap();
        store.record(NewGap::new(GapKind::FolderNotAllowed, "e:/work", "The folder E:/work is outside the folders Aqua may use"), in_window).unwrap();
        // A grant that was applied (and cannot be taken back), a flow change that was applied and undone, a rejected one, and one so old that it is not in the window.
        let mk = |gap_id: i64, kind: &str, at: i64| store.proposal_add(NewProposal { gap_id, kind: kind.into(), title: format!("{kind} proposal"), reason: "r".into(), dry_run: "d".into(), payload: json!({ "kind": kind }) }, at).unwrap();
        let allow = mk(old.id, "allow_folder", in_window);
        assert!(store.proposal_claim(allow.id, "ui", in_window).unwrap());
        store.proposal_applied(allow.id, in_window, &json!({ "kind": "revoke_folder", "path": "E:/work", "access": "rw" }), &json!({ "request_id": "req_allow" })).unwrap();
        let off = mk(fresh.id, "switch_off_step", in_window);
        assert!(store.proposal_claim(off.id, "ui", in_window).unwrap());
        store.proposal_applied(off.id, in_window, &json!({ "kind": "flow_version", "flow_id": 1, "restore_version": 1, "applied_version": 2 }), &json!({})).unwrap();
        store.proposal_undone(off.id, in_window + 60, &json!({})).unwrap();
        let no = mk(fresh.id, "switch_platform", in_window);
        assert!(store.proposal_reject(no.id, "no").unwrap());
        let ancient = mk(fresh.id, "switch_platform", NOW - 40 * DAY);
        assert!(store.proposal_reject(ancient.id, "no").unwrap());

        let checks = std::cell::Cell::new(0);
        let verify = || {
            checks.set(checks.get() + 1);
            Ok::<u64, String>(14)
        };
        let has_undo = |run: &str| run == "run_a";
        let r = build(&store, ReportInput { audit_path: &log, days: 7, now: NOW, has_undo: &has_undo, folders_can_be_taken_back: false, verify_chain: Some(&verify) }).unwrap();

        assert_eq!(checks.get(), 1, "the chain is checked once");
        assert_eq!((r["noticed"]["total"].clone(), r["noticed"]["new"].clone(), r["noticed"]["again"].clone()), (json!(2), json!(1), json!(1)), "{r}");
        assert_eq!(r["noticed"]["by_kind"], json!({ "folder_not_allowed": 1, "platform_not_built": 1 }));
        // Looked up: the three web calls and the research call of the window (the old search and the unreadable line are not counted).
        assert_eq!((r["looked_up"]["calls"].clone(), r["looked_up"]["failed"].clone(), r["looked_up"]["runs"].clone()), (json!(4), json!(1), json!(2)), "{}", r["looked_up"]);
        assert_eq!(r["looked_up"]["by_tool"]["web.search"], json!({ "ok": 2, "failed": 0 }));
        // Proposed in the window: three (the ancient one is not).
        assert_eq!(r["proposed"]["total"], 3, "{}", r["proposed"]);
        assert_eq!(r["rejected"], 1);
        // Applied: both, with who approved; the grant shows the tool host's own question.
        let applied = r["applied"].as_array().unwrap();
        assert_eq!(applied.len(), 2, "{applied:?}");
        let grant = applied.iter().find(|a| a["kind"] == "allow_folder").unwrap();
        assert_eq!((grant["approved_by"].clone(), grant["host_asked"].clone()), (json!("ui"), json!({ "kind": "once", "by": "ui" })));
        assert_eq!(r["undone"].as_array().unwrap().len(), 1);
        // No undo: the write of run_c (the undo store has nothing for it), its command, and the folder grant. The write of run_a can be put back; a click of the person is not Aqua's doing.
        let none = &r["changes_without_undo"];
        assert_eq!(none["no_undo"], true);
        let items = none["items"].as_array().unwrap();
        assert_eq!(items.len(), 3, "{none}");
        assert!(items.iter().all(|i| i["no_undo"] == true));
        let tools: BTreeSet<String> = items.iter().map(|i| i["tool"].as_str().unwrap().to_string()).collect();
        assert_eq!(tools, ["allow_folder", "fs.write", "term.run"].iter().map(|s| s.to_string()).collect::<BTreeSet<_>>());
        let write = items.iter().find(|i| i["tool"] == "fs.write").unwrap();
        assert_eq!((write["run_id"].clone(), write["calls"].clone(), write["targets"].clone()), (json!("run_c"), json!(1), json!(["E:/work/b.txt"])));
        assert_eq!(none["count"], 3);
        assert_eq!(r["changes_with_undo"]["calls"], 1);
        // The log: read once, covers the window, chain checked.
        assert_eq!((r["ledger"]["covers_window"].clone(), r["ledger"]["chain"].clone()), (json!(true), json!({ "checked": true, "ok": true, "lines": 14 })));
        let text = r["text"].as_str().unwrap();
        assert!(text.contains("CHANGES WITH NO UNDO (3)") && text.contains("[NO UNDO] fs.write in run run_c"), "{text}");
        assert!(text.contains("the tool host asked again and the answer was once (by ui)"), "{text}");

        // When the tool host can take a folder back, the grant is not in the list; a chain that does not hold is a warning; with no check asked, none is made.
        let broken = || Err::<u64, String>("line 4: previous hash does not match".to_string());
        let r2 = build(&store, ReportInput { audit_path: &log, days: 7, now: NOW, has_undo: &has_undo, folders_can_be_taken_back: true, verify_chain: Some(&broken) }).unwrap();
        assert_eq!(r2["changes_without_undo"]["items"].as_array().unwrap().len(), 2);
        assert_eq!(r2["ledger"]["chain"]["ok"], false);
        assert!(r2["text"].as_str().unwrap().contains("WARNING: the audit log's chain did not check out"), "{}", r2["text"]);
        let r3 = build(&store, ReportInput { audit_path: &log, days: 7, now: NOW, has_undo: &has_undo, folders_can_be_taken_back: true, verify_chain: None }).unwrap();
        assert_eq!(r3["ledger"]["chain"], json!({ "checked": false }));
        // A longer window sees the old search and the ancient proposal.
        let r4 = build(&store, ReportInput { audit_path: &log, days: 60, now: NOW, has_undo: &has_undo, folders_can_be_taken_back: true, verify_chain: None }).unwrap();
        assert_eq!((r4["looked_up"]["calls"].clone(), r4["proposed"]["total"].clone()), (json!(5), json!(4)));
    }

    #[test]
    fn a_missing_log_and_an_empty_store_make_a_report_that_says_nothing_happened() {
        let dir = temp_dir("report_empty");
        let store = RepairStore::new(dir.join("self_repair.db"));
        let none = |_: &str| false;
        let r = build(&store, ReportInput { audit_path: &dir.join("host.audit.jsonl"), days: 0, now: NOW, has_undo: &none, folders_can_be_taken_back: false, verify_chain: None }).unwrap();
        assert_eq!(r["window"]["days"], 1, "a window of no days is one day");
        assert_eq!((r["noticed"]["total"].clone(), r["looked_up"]["calls"].clone(), r["proposed"]["total"].clone(), r["changes_without_undo"]["count"].clone()), (json!(0), json!(0), json!(0), json!(0)));
        let text = r["text"].as_str().unwrap();
        assert!(text.contains("Noticed: nothing") && text.contains("Looked up: nothing.") && text.contains("Changes with no undo: none"), "{text}");
    }

    /// Card OA5-04, TL-15: a look-up that failed for a moment and was made once more is one look-up that worked, not one that failed and one that worked.
    #[test]
    fn the_line_of_a_try_that_was_made_again_is_not_counted_as_a_look_up_that_failed() {
        let dir = temp_dir("report_retry");
        let in_window = NOW - DAY;
        let mut failed_try: Value = serde_json::from_str(&line(1, in_window, "agent", "web.read", "run_a", false, &["NETWORK"], json!({ "url": "https://example.com/a" }), "r1", None)).unwrap();
        failed_try["decision"] = json!("auto:retry");
        failed_try["error_code"] = json!("RATE_LIMITED");
        let lines = [failed_try.to_string(), line(2, in_window, "agent", "web.read", "run_a", true, &["NETWORK"], json!({ "url": "https://example.com/a" }), "r1", None)];
        let log = dir.join("host.audit.jsonl");
        std::fs::write(&log, lines.join("\n") + "\n").unwrap();
        let store = RepairStore::new(dir.join("self_repair.db"));
        let none = |_: &str| false;
        let r = build(&store, ReportInput { audit_path: &log, days: 7, now: NOW, has_undo: &none, folders_can_be_taken_back: false, verify_chain: None }).unwrap();
        assert_eq!((r["looked_up"]["calls"].clone(), r["looked_up"]["failed"].clone()), (json!(1), json!(0)), "{}", r["looked_up"]);
        assert_eq!(r["looked_up"]["by_tool"]["web.read"], json!({ "ok": 1, "failed": 0 }));
    }
}
