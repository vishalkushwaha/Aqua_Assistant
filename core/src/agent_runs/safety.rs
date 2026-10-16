//! What a run could read, change and send, in plain words: the "lethal trifecta" of an agent that has read text from outside (which may be steering it),
//! holds data that is the person's own, and has a way to change things or send them out. Any two are manageable; all three together are what an
//! attacker needs, and the run says which it held and when it came to hold each.
//!
//! It is worked out from the run's own record (what it held, and every call it made), not from what the run says about itself.

use serde::Serialize;
use serde_json::Value;

/// One of the properties: whether the run held it, and where in the run that began.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Property {
    pub held: bool,
    /// The run took this on after this many of its calls had come back (0: before any).
    pub after_call: Option<u32>,
    /// When, in milliseconds since 1970 (what the record kept).
    pub at_ms: Option<i64>,
    /// For the two that are about tools: which tools of the run it is about.
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Safety {
    pub run_id: String,
    /// The run read text from outside (a page, a file, a window): text that can try to steer it.
    pub untrusted_input: Property,
    /// The run held data that is the person's (the screen, the clipboard, a file outside the project).
    pub private_data: Property,
    /// The run used a tool that can change something.
    pub can_change: Property,
    /// The run used a tool that reaches out (the internet, another machine, another system).
    pub can_send: Property,
    /// How many of the four parts the run held, counting the two tools as one reach.
    pub parts_held: u8,
    /// All three of: text from outside, the person's data, and a way out.
    pub all_three: bool,
    /// What it comes to, in a sentence or two.
    pub summary: String,
}

/// What the record kept about what a run held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarksDetail {
    pub tainted: bool,
    pub private: bool,
    pub tainted_at: Option<i64>,
    pub private_at: Option<i64>,
}

/// The calls of a run that came back, oldest first: (tool, when it was written in ms).
fn calls(run: &Value) -> Vec<(String, i64)> {
    run["steps"].as_array().map(|s| s.iter().filter(|x| x["kind"] == "tool").map(|x| (x["tool"].as_str().unwrap_or("").to_string(), x["ts"].as_i64().unwrap_or(0))).collect()).unwrap_or_default()
}

fn after_call(calls: &[(String, i64)], at: i64) -> u32 {
    calls.iter().filter(|(_, ts)| *ts <= at).count() as u32
}

/// How the calls that change or send were allowed, counted from the run's own calls (card R6-12): the summary used to say "so anything that changes or sends asked every time",
/// which is false for a call the rules allowed, for one that an earlier answer of the session covered, and for a browser step under full access.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HowAllowed {
    total: usize,
    /// The person was asked about this call and answered (yes, always, or no).
    asked: usize,
    /// Of those, the ones the person refused.
    refused: usize,
    /// An answer given earlier in the session covered the call.
    earlier_answer: usize,
    /// The person allowed this run's project checks in this folder ahead of time (card OA10-05): not a question, and not a call the rules let through.
    preapproved: usize,
    /// Nobody was asked: the rules allowed it.
    unasked: usize,
    unasked_tools: Vec<String>,
}

fn how_allowed(run: &Value, reaches: &dyn Fn(&str) -> bool) -> HowAllowed {
    let mut h = HowAllowed::default();
    for s in run["steps"].as_array().map(|s| s.as_slice()).unwrap_or(&[]) {
        let tool = s["tool"].as_str().unwrap_or("");
        if s["kind"] != "tool" || !reaches(tool) {
            continue;
        }
        h.total += 1;
        match s["approval"].as_str().unwrap_or("") {
            "" => {
                h.unasked += 1;
                if !h.unasked_tools.iter().any(|t| t == tool) {
                    h.unasked_tools.push(tool.to_string());
                }
            }
            "approved_session" => h.earlier_answer += 1,
            "preapproved" => h.preapproved += 1,
            "denied" | "timeout" | "cancelled" => {
                h.asked += 1;
                h.refused += 1;
            }
            _ => h.asked += 1,
        }
    }
    h
}

/// The sentence about it, with a space in front; empty when the run made no such call.
fn how_allowed_sentence(h: &HowAllowed) -> String {
    if h.total == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    if h.asked > 0 {
        parts.push(format!("{} asked you first{}", h.asked, if h.refused > 0 { format!(" (you refused {})", h.refused) } else { String::new() }));
    }
    if h.earlier_answer > 0 {
        parts.push(format!("{} went on an answer you gave earlier in the session", h.earlier_answer));
    }
    if h.preapproved > 0 {
        parts.push(format!("{} were project checks that you allowed for this run ahead of time", h.preapproved));
    }
    if h.unasked > 0 {
        parts.push(format!("{} ran without a question ({})", h.unasked, h.unasked_tools.join(", ")));
    }
    let list = match parts.len() {
        0 | 1 => parts.join(""),
        n => format!("{} and {}", parts[..n - 1].join(", "), parts[n - 1]),
    };
    format!(" Of its {} call{} that change or send, {list}.", h.total, if h.total == 1 { "" } else { "s" })
}

/// `run` is the record as `AgentRunStore::get` gives it (with its steps). `changes` and `sends` say what a tool can do (see `agent_tools`).
pub fn assess(run: &Value, marks: Option<MarksDetail>, changes: &dyn Fn(&str) -> bool, sends: &dyn Fn(&str) -> bool) -> Safety {
    let all = calls(run);
    let marks = marks.unwrap_or_default();
    let prop_of_mark = |held: bool, at: Option<i64>| Property { held, after_call: at.map(|a| after_call(&all, a)), at_ms: at, tools: Vec::new() };
    let tool_prop = |pred: &dyn Fn(&str) -> bool| {
        let used: Vec<&(String, i64)> = all.iter().filter(|(t, _)| pred(t)).collect();
        let mut tools: Vec<String> = Vec::new();
        for (t, _) in &used {
            if !tools.contains(t) {
                tools.push(t.clone());
            }
        }
        let first = used.first();
        Property { held: !used.is_empty(), after_call: first.map(|(_, ts)| after_call(&all, *ts).saturating_sub(1)), at_ms: first.map(|(_, ts)| *ts), tools }
    };
    let untrusted = prop_of_mark(marks.tainted, marks.tainted_at);
    let private = prop_of_mark(marks.private, marks.private_at);
    let can_change = tool_prop(changes);
    let can_send = tool_prop(sends);
    let reach = can_change.held || can_send.held;
    let all_three = untrusted.held && private.held && reach;
    let parts_held = u8::from(untrusted.held) + u8::from(private.held) + u8::from(reach);
    let mut parts = Vec::new();
    if untrusted.held {
        parts.push(format!("read text from outside{}", untrusted.after_call.map(|n| format!(" (after call {n})")).unwrap_or_default()));
    }
    if private.held {
        parts.push(format!("held data of yours{}", private.after_call.map(|n| format!(" (after call {n})")).unwrap_or_default()));
    }
    if can_change.held {
        parts.push(format!("used tools that change things ({})", can_change.tools.join(", ")));
    }
    if can_send.held {
        parts.push(format!("used tools that reach out ({})", can_send.tools.join(", ")));
    }
    // What was asked is counted from the run's own calls, never said in general.
    let how = how_allowed_sentence(&how_allowed(run, &|t| changes(t) || sends(t)));
    let summary = if parts.is_empty() {
        "This run read nothing from outside, held none of your data and used no tool that changes or sends anything.".to_string()
    } else if all_three {
        format!("This run {}. All three of the dangerous combination were there (text from outside, your data, a way out).{how}", parts.join("; "))
    } else {
        let missing: Vec<&str> = [("text from outside", untrusted.held), ("data of yours", private.held), ("way to change or send", reach)].iter().filter(|(_, h)| !h).map(|(n, _)| *n).collect();
        format!("This run {}. It had no {}, so the dangerous combination was not complete.{how}", parts.join("; "), missing.join(" and no "))
    };
    Safety { run_id: run["id"].as_str().unwrap_or("").to_string(), untrusted_input: untrusted, private_data: private, can_change, can_send, parts_held, all_three, summary }
}

/// One page a person can show somebody: what the run was for, how it ended, what it could read, change and send, and what was asked of the person.
pub fn one_page(run: &Value, safety: &Safety, proof_line: Option<&str>) -> String {
    let steps = run["steps"].as_array().cloned().unwrap_or_default();
    let tool_steps: Vec<&Value> = steps.iter().filter(|s| s["kind"] == "tool").collect();
    let mut out = String::new();
    out.push_str(&format!("# Run {}\n\n", safety.run_id));
    out.push_str(&format!("- Goal: {}\n", run["goal"].as_str().unwrap_or("").replace('\n', " ")));
    out.push_str(&format!("- Ended: {} ({}), after {} calls\n", run["status"].as_str().unwrap_or("?"), run["stopped_reason"].as_str().unwrap_or(""), tool_steps.len()));
    out.push_str(&format!("\n## What it could read, change and send\n\n{}\n", safety.summary));
    out.push_str(&format!("\n- Text from outside: {}\n- Your data: {}\n- Tools that change things: {}\n- Tools that reach out: {}\n", yn(&safety.untrusted_input), yn(&safety.private_data), yn(&safety.can_change), yn(&safety.can_send)));
    // A call that an answer from earlier in the session covered was not a question put to the person this time.
    let asked: Vec<&&Value> = tool_steps.iter().filter(|s| !matches!(s["approval"].as_str().unwrap_or(""), "" | "approved_session" | "preapproved")).collect();
    let on_earlier_answer = tool_steps.iter().filter(|s| s["approval"].as_str() == Some("approved_session")).count();
    let preapproved = tool_steps.iter().filter(|s| s["approval"].as_str() == Some("preapproved")).count();
    out.push_str(&format!("\n## Questions asked of the person: {}\n\n", asked.len()));
    for s in asked {
        out.push_str(&format!("- {} — answered: {}\n", s["tool"].as_str().unwrap_or("?"), s["approval"].as_str().unwrap_or("")));
    }
    if on_earlier_answer > 0 {
        out.push_str(&format!("- {on_earlier_answer} more call{} went on an answer given earlier in the session\n", if on_earlier_answer == 1 { "" } else { "s" }));
    }
    if preapproved > 0 {
        out.push_str(&format!("- {preapproved} more call{} (project checks) ran on a pre-approval you made for this run, without a question\n", if preapproved == 1 { "" } else { "s" }));
    }
    out.push_str("\n## Calls, in order\n\n");
    for (i, s) in tool_steps.iter().enumerate() {
        out.push_str(&format!("{}. {} — {} ({})\n", i + 1, s["tool"].as_str().unwrap_or("?"), s["status"].as_str().unwrap_or("?"), s["decision"].as_str().unwrap_or("")));
    }
    if let Some(p) = proof_line {
        out.push_str(&format!("\n## Record\n\n{p}\n"));
    }
    out
}

fn yn(p: &Property) -> String {
    if !p.held {
        return "no".into();
    }
    if p.tools.is_empty() {
        "yes".into()
    } else {
        format!("yes ({})", p.tools.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(tools: &[(&str, i64, &str)]) -> Value {
        json!({
            "id": "run_1", "goal": "do the thing", "status": "completed", "stopped_reason": "final",
            "steps": tools.iter().map(|(t, ts, approval)| json!({"kind": "tool", "tool": t, "ts": ts, "status": "completed", "decision": "allowed", "approval": approval})).collect::<Vec<_>>(),
        })
    }

    fn changes(t: &str) -> bool {
        matches!(t, "fs.write" | "term.run")
    }

    fn sends(t: &str) -> bool {
        matches!(t, "web.read" | "http.request")
    }

    #[test]
    fn a_run_that_only_looked_at_its_own_files_holds_none_of_the_three() {
        let s = assess(&run(&[("fs.read", 10, ""), ("fs.list", 20, "")]), None, &changes, &sends);
        assert_eq!((s.parts_held, s.all_three), (0, false));
        assert!(s.summary.starts_with("This run read nothing from outside, held none of your data"), "{}", s.summary);
    }

    #[test]
    fn each_part_says_when_it_was_taken_on_and_all_three_together_are_said_plainly() {
        let r = run(&[("fs.read", 10, ""), ("web.read", 20, ""), ("fs.write", 30, "approved"), ("fs.read", 40, "")]);
        let marks = MarksDetail { tainted: true, private: true, tainted_at: Some(21), private_at: Some(35) };
        let s = assess(&r, Some(marks), &changes, &sends);
        assert_eq!(s.untrusted_input.after_call, Some(2), "two calls had come back when the text from outside arrived");
        assert_eq!(s.private_data.after_call, Some(3));
        assert_eq!((s.can_send.tools.clone(), s.can_send.after_call), (vec!["web.read".to_string()], Some(1)), "the way out was first used by the second call");
        assert_eq!(s.can_change.tools, vec!["fs.write".to_string()]);
        assert!(s.all_three && s.parts_held == 3);
        assert!(s.summary.contains("read text from outside (after call 2)") && s.summary.contains("All three of the dangerous combination"), "{}", s.summary);
    }

    /// Card R6-12: "so anything that changes or sends asked every time" was a fixed sentence, false for a call the rules allowed and for one an earlier answer covered.
    #[test]
    fn the_sentence_about_what_asked_is_counted_from_the_calls_and_never_says_every_time() {
        let r = run(&[("fs.read", 10, ""), ("web.read", 20, ""), ("fs.write", 30, "approved"), ("fs.write", 40, "approved_session"), ("term.run", 50, ""), ("http.request", 60, "denied")]);
        let marks = MarksDetail { tainted: true, private: true, tainted_at: Some(15), private_at: Some(16) };
        let s = assess(&r, Some(marks), &changes, &sends);
        assert!(s.all_three);
        assert!(
            s.summary.contains("Of its 5 calls that change or send, 2 asked you first (you refused 1), 1 went on an answer you gave earlier in the session and 2 ran without a question (web.read, term.run)."),
            "{}",
            s.summary
        );
        assert!(!s.summary.contains("every time"), "{}", s.summary);
        // One call, asked: said in the singular.
        let s = assess(&run(&[("fs.write", 10, "approved")]), None, &changes, &sends);
        assert!(s.summary.contains("Of its 1 call that change or send, 1 asked you first."), "{}", s.summary);
        // No call that changes or sends: no such sentence.
        let s = assess(&run(&[("fs.read", 10, "")]), Some(MarksDetail { tainted: true, tainted_at: Some(5), ..Default::default() }), &changes, &sends);
        assert!(!s.summary.contains("Of its"), "{}", s.summary);
        // The one page does not count an earlier answer as a question.
        let r = run(&[("fs.write", 10, "approved"), ("fs.write", 20, "approved_session")]);
        let page = one_page(&r, &assess(&r, None, &changes, &sends), None);
        assert!(page.contains("## Questions asked of the person: 1") && page.contains("1 more call went on an answer given earlier in the session"), "{page}");
    }

    #[test]
    fn two_of_three_says_which_one_was_missing() {
        let r = run(&[("web.read", 10, "")]);
        let s = assess(&r, Some(MarksDetail { tainted: true, ..Default::default() }), &changes, &sends);
        assert!(!s.all_three && s.parts_held == 2);
        assert!(s.summary.contains("It had no data of yours, so the dangerous combination was not complete"), "{}", s.summary);
        // Held data but no way out.
        let s = assess(&run(&[("fs.read", 10, "")]), Some(MarksDetail { private: true, private_at: Some(11), ..Default::default() }), &changes, &sends);
        assert!(s.summary.contains("It had no text from outside and no way to change or send"), "{}", s.summary);
        assert_eq!(s.private_data.after_call, Some(1));
    }

    #[test]
    fn the_one_page_summary_has_the_goal_the_questions_and_the_calls() {
        let r = run(&[("fs.read", 10, ""), ("fs.write", 30, "approved")]);
        let s = assess(&r, None, &changes, &sends);
        let page = one_page(&r, &s, Some("Signed under 2 tree heads."));
        assert!(page.starts_with("# Run run_1") && page.contains("- Goal: do the thing") && page.contains("## Questions asked of the person: 1"), "{page}");
        assert!(page.contains("- fs.write — answered: approved") && page.contains("1. fs.read") && page.contains("2. fs.write") && page.contains("Signed under 2 tree heads."), "{page}");
        assert!(page.contains("Tools that change things: yes (fs.write)") && page.contains("Text from outside: no"));
    }
}
