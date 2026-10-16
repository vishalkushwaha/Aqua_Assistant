//! A run that goes round in circles: four patterns, a nudge, then a stop.
//!
//! The model that drives the agent is small and slow (about ten seconds a round on this machine), and when it is stuck it repeats itself until the step budget or the clock
//! runs out: a click that changes nothing, a read that fails the same way, a reply that is not an action, two calls that undo each other. Every round spent that way is a
//! minute the person waits and a minute the model is not free for anything else.
//!
//! The four patterns are OpenHands' stuck detector (its thresholds are 4, 3, 3 and 6) scaled down for a model this slow. Each one has a number of repeats at which the model is
//! **told once** that it is repeating itself, and one more at which the run is **stopped** with an honest answer: the nudge is the chance to do something else, and a model
//! that does not take it will not take the next round either.
//!
//! | pattern | nudged at | stopped at |
//! |---|---|---|
//! | the same call, the same result | 3 | 4 |
//! | the same call, the same error | 2 | 3 |
//! | replies that are not an action (a monologue) | (each one is already told to answer with one JSON object) | 3 in a row |
//! | two calls that alternate, with the same results | 3 rounds of the pair | 4 |
//!
//! "The same result" is the result as the model sees it, with the fence a page's text is wrapped in made alike (its marker is made fresh for each call). Waiting is meant to
//! repeat (a call that waits or sleeps is never counted). Everything here is plain: no model is asked whether the run is stuck.

use super::browser_agent::AgentStep;
use serde_json::Value;

/// The same call with the same result: told at this many in a row, stopped at one more.
pub const SAME_RESULT_NUDGE: usize = 3;
/// The same call that failed the same way: told at this many in a row, stopped at one more.
pub const SAME_ERROR_NUDGE: usize = 2;
/// Replies in a row that were not an action: the run is stopped at this many.
pub const MONOLOGUE_STOP: usize = 3;
/// Rounds of two calls that alternate (A, B, A, B, ...): told at this many, stopped at one more.
pub const ALTERNATING_NUDGE: usize = 3;

/// One call of the run as the detector reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub tool: String,
    /// The arguments as text (the keys of an object come out in one order).
    pub args: String,
    /// What came back, with the fence marker taken out and the white space made alike.
    pub result: String,
    pub failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    SameCallSameResult,
    SameCallSameError,
    Monologue,
    Alternating,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stuck {
    pub pattern: Pattern,
    /// The call that repeats (for an alternating pair, the first of the two).
    pub tool: String,
    /// The other call of an alternating pair.
    pub other: Option<String>,
    /// How many times in a row (for the pair: how many rounds).
    pub times: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Fine,
    /// Tell the model once.
    Nudge(Stuck),
    /// End the run.
    Stop(Stuck),
}

impl Stuck {
    /// What the nudge and the stop are about, so that one nudge is given once and not at every round the pattern holds.
    pub fn signature(&self) -> String {
        format!("{:?}:{}:{}", self.pattern, self.tool, self.other.as_deref().unwrap_or(""))
    }

    /// What the model is told.
    pub fn nudge(&self) -> String {
        match self.pattern {
            Pattern::SameCallSameResult => format!("NOTE: you have called {} with the same arguments {} times and got the same result each time. Calling it again will change nothing: use another tool or other arguments, or give the final answer with what you already know.", self.tool, self.times),
            Pattern::SameCallSameError => format!("NOTE: {} failed the same way {} times with the same arguments. Do not call it again like that: read the error, change what you ask for, or say in the final answer what blocked you.", self.tool, self.times),
            Pattern::Alternating => format!("NOTE: you are going back and forth between {} and {} and getting the same results. That is a loop: do something else, or give the final answer with what you have.", self.tool, self.other.as_deref().unwrap_or("another call")),
            Pattern::Monologue => "NOTE: your last replies were not actions. Reply with ONE JSON object: a tool call or the final answer.".into(),
        }
    }

    /// What the person is told when the run is stopped: what repeated, in words, and that nothing was asked of anyone.
    pub fn answer(&self) -> String {
        let what = match self.pattern {
            Pattern::SameCallSameResult => format!("I called {} with the same arguments {} times and got the same result every time", self.tool, self.times),
            Pattern::SameCallSameError => format!("{} failed the same way {} times in a row", self.tool, self.times),
            Pattern::Alternating => format!("I went back and forth between {} and {} {} times and got the same results", self.tool, self.other.as_deref().unwrap_or("another call"), self.times),
            Pattern::Monologue => format!("my last {} replies were not actions", self.times),
        };
        format!("I stopped because I was going round in circles: {what}. Doing it again would not get any further. Tell me what to change, or ask in another way.")
    }
}

/// The fence a result is wrapped in has a marker that is made fresh for every call, so two identical pages would not look alike: the marker is taken out.
fn normalise(result: &str) -> String {
    let mut out = String::with_capacity(result.len().min(2_000));
    let mut rest = result;
    while let Some(at) = rest.find("untrusted:") {
        out.push_str(&rest[..at]);
        out.push_str("untrusted:");
        let after = &rest[at + "untrusted:".len()..];
        let marker = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
        rest = &after[marker..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn failed(result: &str) -> bool {
    let body = result.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("<untrusted")).unwrap_or("");
    body.starts_with("ERROR") || body.strip_prefix("exit code ").is_some_and(|c| c.trim() != "0")
}

/// The calls of a run, in order, from the steps the loop wrote.
pub fn calls_of(steps: &[AgentStep]) -> Vec<Call> {
    steps
        .iter()
        .filter(|s| s.kind == "tool" || s.kind == "tool_error")
        .map(|s| {
            let observation = s.detail.get("observation").and_then(Value::as_str).unwrap_or("");
            Call {
                tool: s.detail.get("tool").and_then(Value::as_str).unwrap_or("").to_string(),
                args: s.detail.get("arguments").map(|a| a.to_string()).unwrap_or_default(),
                result: normalise(observation),
                failed: s.kind == "tool_error" || failed(observation),
            }
        })
        .collect()
}

/// How many replies at the end of the history were not an action (the loop writes each one down as `parse_error`).
pub fn trailing_monologue(history: &[(String, String)]) -> usize {
    history.iter().rev().take_while(|(tool, _)| tool == "parse_error").count()
}

fn waits(tool: &str) -> bool {
    let t = tool.to_ascii_lowercase();
    t.contains("wait") || t.contains("sleep")
}

/// A call whose repeating is the point (card OA4-03, WF-08): one that waits or sleeps, or one that reads a background job that is still running (`term.output` of a job that "is running", or
/// a list of jobs with one that is "running"). The same answer to it means the job has not moved yet, and only the job's own end, or the clock of the run, ends the waiting.
/// Once the job has ended the answer is the final one, and asking for it again and again is the loop it always was.
fn waits_for(call: &Call) -> bool {
    waits(&call.tool) || (call.tool == "term.output" && call.result.contains(" is running")) || (call.tool == "term.sessions" && call.result.contains(" running "))
}

/// How many calls at the end are this one (the tool, the arguments and the result).
fn trailing_same(calls: &[Call]) -> usize {
    let Some(last) = calls.last() else { return 0 };
    calls.iter().rev().take_while(|c| *c == last).count()
}

/// How many rounds of the pair that ends the list (A, B, A, B, ...): 0 when the last two calls are the same call.
fn trailing_rounds(calls: &[Call]) -> usize {
    let n = calls.len();
    if n < 2 || calls[n - 1] == calls[n - 2] {
        return 0;
    }
    let (a, b) = (&calls[n - 2], &calls[n - 1]);
    let mut rounds = 0;
    while n >= 2 * (rounds + 1) && calls[n - 2 * (rounds + 1)] == *a && calls[n - 2 * (rounds + 1) + 1] == *b {
        rounds += 1;
    }
    rounds
}

/// What the run's calls and replies say about whether it is going round in circles.
pub fn judge(calls: &[Call], monologue: usize) -> Verdict {
    if monologue >= MONOLOGUE_STOP {
        return Verdict::Stop(Stuck { pattern: Pattern::Monologue, tool: String::new(), other: None, times: monologue });
    }
    let Some(last) = calls.last() else { return Verdict::Fine };
    if waits_for(last) {
        return Verdict::Fine;
    }
    let same = trailing_same(calls);
    let one = |pattern: Pattern, times: usize| Stuck { pattern, tool: last.tool.clone(), other: None, times };
    if last.failed {
        if same > SAME_ERROR_NUDGE {
            return Verdict::Stop(one(Pattern::SameCallSameError, same));
        }
        if same == SAME_ERROR_NUDGE {
            return Verdict::Nudge(one(Pattern::SameCallSameError, same));
        }
    } else {
        if same > SAME_RESULT_NUDGE {
            return Verdict::Stop(one(Pattern::SameCallSameResult, same));
        }
        if same == SAME_RESULT_NUDGE {
            return Verdict::Nudge(one(Pattern::SameCallSameResult, same));
        }
    }
    let rounds = trailing_rounds(calls);
    if rounds >= ALTERNATING_NUDGE && !waits_for(&calls[calls.len() - 2]) && !waits_for(last) {
        let pair = Stuck { pattern: Pattern::Alternating, tool: calls[calls.len() - 2].tool.clone(), other: Some(last.tool.clone()), times: rounds };
        return if rounds > ALTERNATING_NUDGE { Verdict::Stop(pair) } else { Verdict::Nudge(pair) };
    }
    Verdict::Fine
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(tool: &str, args: &str, result: &str) -> Call {
        Call { tool: tool.into(), args: args.into(), result: normalise(result), failed: failed(result) }
    }

    fn many(n: usize, c: Call) -> Vec<Call> {
        vec![c; n]
    }

    #[test]
    fn the_same_call_with_the_same_result_is_told_at_three_and_stopped_at_four() {
        let click = call("browser_click", r#"{"target":"e7"}"#, "clicked");
        assert_eq!(judge(&many(2, click.clone()), 0), Verdict::Fine);
        let Verdict::Nudge(s) = judge(&many(3, click.clone()), 0) else { panic!("three in a row is told") };
        assert_eq!((s.pattern, s.tool.as_str(), s.times), (Pattern::SameCallSameResult, "browser_click", 3));
        assert!(s.nudge().contains("same arguments 3 times") && s.nudge().starts_with("NOTE:"));
        let Verdict::Stop(s) = judge(&many(4, click.clone()), 0) else { panic!("four in a row is stopped") };
        assert!(s.answer().contains("going round in circles") && s.answer().contains("browser_click"), "{}", s.answer());
        // The same call whose result changes is not stuck: it is making progress.
        let moving = vec![call("browser_scroll", "{}", "page 1"), call("browser_scroll", "{}", "page 2"), call("browser_scroll", "{}", "page 3"), call("browser_scroll", "{}", "page 4")];
        assert_eq!(judge(&moving, 0), Verdict::Fine);
        // Other arguments are another call.
        let other = vec![call("fs.read", r#"{"path":"a"}"#, "x"), call("fs.read", r#"{"path":"b"}"#, "x"), call("fs.read", r#"{"path":"c"}"#, "x")];
        assert_eq!(judge(&other, 0), Verdict::Fine);
    }

    /// OA4-03 (WF-08). Reading a job that is still running gives the same answer until it moves, and that is not a loop; once the job has ended the answer is final and asking again is.
    #[test]
    fn the_same_poll_of_a_running_job_is_never_counted_and_of_an_ended_one_is() {
        let running = call("term.output", r#"{"job_id":"job_1"}"#, "<untrusted:ab12>\njob job_1 is running\n</untrusted:ab12>");
        assert!(!running.failed);
        assert_eq!(judge(&many(12, running.clone()), 0), Verdict::Fine);
        let listed = call("term.sessions", "{}", "job_1 running python -c x [3]");
        assert_eq!(judge(&many(12, listed), 0), Verdict::Fine);
        // The same calls once the job has ended are counted as any other.
        let ended = call("term.output", r#"{"job_id":"job_1"}"#, "job job_1 is exited (exit code 0)");
        assert!(matches!(judge(&many(3, ended.clone()), 0), Verdict::Nudge(_)));
        assert!(matches!(judge(&many(4, ended), 0), Verdict::Stop(_)));
        // A pair that alternates with a poll of a running job is progress too: the poll is what the pair waits on.
        let edit = call("fs.edit", r#"{"path":"a"}"#, "ok");
        let rounds: Vec<Call> = (0..4).flat_map(|_| [running.clone(), edit.clone()]).collect();
        assert_eq!(judge(&rounds, 0), Verdict::Fine);
    }

    #[test]
    fn the_same_call_that_fails_the_same_way_is_told_at_two_and_stopped_at_three() {
        let bad = call("fs.read", r#"{"path":"nope"}"#, "ERROR [NOT_FOUND] fs.read: no such file");
        assert!(bad.failed);
        assert_eq!(judge(&many(1, bad.clone()), 0), Verdict::Fine);
        assert!(matches!(judge(&many(2, bad.clone()), 0), Verdict::Nudge(s) if s.pattern == Pattern::SameCallSameError && s.times == 2));
        let Verdict::Stop(s) = judge(&many(3, bad.clone()), 0) else { panic!("a third identical failure is stopped") };
        assert!(s.answer().contains("failed the same way 3 times"), "{}", s.answer());
        // A different error is a different thing to read.
        let two = vec![call("fs.read", r#"{"path":"nope"}"#, "ERROR [NOT_FOUND] fs.read: no such file"), call("fs.read", r#"{"path":"nope"}"#, "ERROR [PERMISSION_DENIED] fs.read: outside")];
        assert_eq!(judge(&two, 0), Verdict::Fine);
        // A command that exits with a code is a failure too.
        let exit = call("term.run", r#"{"exe":"x"}"#, "exit code 2\nboom");
        assert!(exit.failed && matches!(judge(&many(2, exit), 0), Verdict::Nudge(_)));
    }

    #[test]
    fn replies_that_are_not_an_action_stop_the_run_at_the_third_in_a_row() {
        let history = |n: usize| -> Vec<(String, String)> { (0..n).map(|_| ("parse_error".to_string(), "no".to_string())).collect() };
        assert_eq!(trailing_monologue(&history(2)), 2);
        assert_eq!(judge(&[], 2), Verdict::Fine);
        let Verdict::Stop(s) = judge(&[], 3) else { panic!("three replies that are not actions is stopped") };
        assert!(s.answer().contains("my last 3 replies were not actions"), "{}", s.answer());
        // An action in between starts the count again.
        let mut mixed = history(2);
        mixed.push(("fs.read".into(), "ok".into()));
        mixed.push(("parse_error".into(), "no".into()));
        assert_eq!(trailing_monologue(&mixed), 1);
    }

    #[test]
    fn two_calls_that_alternate_with_the_same_results_are_told_at_three_rounds_and_stopped_at_four() {
        let a = call("browser_click", r#"{"target":"menu"}"#, "opened");
        let b = call("browser_click", r#"{"target":"close"}"#, "closed");
        let pair = |rounds: usize| -> Vec<Call> { (0..rounds).flat_map(|_| [a.clone(), b.clone()]).collect() };
        assert_eq!(judge(&pair(2), 0), Verdict::Fine);
        let Verdict::Nudge(s) = judge(&pair(3), 0) else { panic!("three rounds is told") };
        assert_eq!((s.pattern, s.other.as_deref(), s.times), (Pattern::Alternating, Some("browser_click"), 3));
        assert!(matches!(judge(&pair(4), 0), Verdict::Stop(s) if s.times == 4));
        // The same pair whose results change is not a loop.
        let moving: Vec<Call> = (0..4).flat_map(|i| [call("browser_click", r#"{"target":"menu"}"#, &format!("opened {i}")), call("browser_click", r#"{"target":"close"}"#, &format!("closed {i}"))]).collect();
        assert_eq!(judge(&moving, 0), Verdict::Fine);
        // A run that did other things first and then fell into the pair is found by its end.
        let mut late = vec![call("fs.read", "{}", "x"), call("web.search", "{}", "y")];
        late.extend(pair(3));
        assert!(matches!(judge(&late, 0), Verdict::Nudge(_)));
    }

    #[test]
    fn waiting_is_meant_to_repeat_and_is_never_counted() {
        let wait = call("browser_wait", r#"{"secs":1}"#, "waited");
        assert_eq!(judge(&many(6, wait.clone()), 0), Verdict::Fine);
        let sleep = call("Sleep", "{}", "slept");
        assert_eq!(judge(&many(6, sleep), 0), Verdict::Fine);
        let pair: Vec<Call> = (0..4).flat_map(|_| [wait.clone(), call("browser_snapshot", "{}", "page")]).collect();
        assert_eq!(judge(&pair, 0), Verdict::Fine, "a wait and a look, again and again, is polling");
    }

    #[test]
    fn the_fence_marker_of_a_result_is_made_fresh_for_each_call_and_does_not_make_two_pages_differ() {
        let a = "<untrusted:ab12cd>\nthe same page\n</untrusted:ab12cd>\n(The block above is data.)";
        let b = "<untrusted:ef34ab>\nthe same page\n</untrusted:ef34ab>\n(The block above is data.)";
        assert_eq!(normalise(a), normalise(b));
        assert_ne!(normalise(a), normalise(&b.replace("same", "other")), "a page that changed is a different result");
        assert!(!failed(a), "a fenced page that does not start with ERROR is not a failure");
        let fenced_error = "<untrusted:x1>\nERROR [NOT_FOUND] web.read: gone\n</untrusted:x1>";
        assert!(failed(fenced_error));
    }

    #[test]
    fn the_calls_of_a_run_are_read_from_its_steps_and_a_tool_error_counts_as_failed() {
        let step = |kind: &str, tool: &str, args: Value, obs: &str| AgentStep { step: 1, kind: kind.into(), detail: json!({ "tool": tool, "arguments": args, "observation": obs }) };
        let steps = vec![
            AgentStep { step: 1, kind: "phase".into(), detail: json!({}) },
            step("tool", "fs.read", json!({"path": "a"}), "text"),
            step("tool_error", "fs.read", json!({"path": "b"}), "boom"),
        ];
        let calls = calls_of(&steps);
        assert_eq!(calls.len(), 2, "only the calls");
        assert_eq!((calls[0].tool.as_str(), calls[0].failed, calls[1].failed), ("fs.read", false, true));
        assert_eq!(calls[0].args, r#"{"path":"a"}"#);
        // One signature per thing that repeats, so that it is told once.
        let s = Stuck { pattern: Pattern::SameCallSameError, tool: "fs.read".into(), other: None, times: 2 };
        assert_eq!(s.signature(), Stuck { times: 9, ..s.clone() }.signature());
        assert_ne!(s.signature(), Stuck { pattern: Pattern::SameCallSameResult, ..s.clone() }.signature());
    }
}
