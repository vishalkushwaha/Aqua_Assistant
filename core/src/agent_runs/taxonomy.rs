//! What goes wrong with tools, by kind, tracked per tool. A quality number that only says "the task failed" does not say what to fix; this says whether the
//! model sent arguments the tool did not accept, the world was not as expected (a file missing, a program not installed, a timeout), the tool or the service
//! behind it failed, or the person or the policy said no. A run that ended because the model's reply could not be read is counted for the planner.
//!
//! It is worked out from the run records (the first 500 characters of each result, which start with `ERROR [CODE] tool: message` when a call failed).

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The arguments were not what the tool takes: the model's fault, or the tool's description.
    InvalidArguments,
    /// The world was not as expected: not found, not available, timed out, cancelled.
    UnexpectedEnvironment,
    /// The tool or the service behind it failed.
    ProviderError,
    /// The person, the policy, a skill or a hook said no. Not a failure of the tool.
    Refused,
    /// The model's reply could not be read (counted for the planner, not for a tool).
    ParseFailure,
    /// An error with a code this does not know.
    Other,
}

/// The kind of a failed call from its result, or `None` when the result is not an error.
pub fn classify(observation: &str) -> Option<ErrorKind> {
    let rest = observation.trim_start().strip_prefix("ERROR")?;
    let code = rest.trim_start().strip_prefix('[').and_then(|r| r.split_once(']')).map(|(c, _)| c.trim()).unwrap_or("");
    Some(match code {
        "INVALID_ARGUMENTS" => ErrorKind::InvalidArguments,
        "NOT_FOUND" | "UNAVAILABLE" | "TIMEOUT" | "CANCELLED" | "RATE_LIMITED" | "APPROVAL_TIMEOUT" => ErrorKind::UnexpectedEnvironment,
        "BACKEND_ERROR" | "INTERNAL" | "PROTOCOL_ERROR" => ErrorKind::ProviderError,
        "PERMISSION_DENIED" | "APPROVAL_DENIED" | "CONFIRMATION_DENIED" | "NEEDS_USER_CHOICE" | "SECRET_BLOCKED" => ErrorKind::Refused,
        c if c.starts_with("POLICY_") => ErrorKind::Refused,
        _ => ErrorKind::Other,
    })
}

/// Per tool, how many calls ended in each kind of error.
pub type Tally = BTreeMap<String, BTreeMap<ErrorKind, u32>>;

/// Counts from (tool, result) pairs. A call whose result is not an error is not counted here (the totals are in `calls`).
pub fn tally<'a>(calls: impl IntoIterator<Item = (&'a str, &'a str)>) -> (Tally, u32) {
    let mut out: Tally = BTreeMap::new();
    let mut total = 0;
    for (tool, observation) in calls {
        total += 1;
        if let Some(kind) = classify(observation) {
            *out.entry(tool.to_string()).or_default().entry(kind).or_insert(0) += 1;
        }
    }
    (out, total)
}

/// How a run's stop reason counts: the planner's reply that could not be read.
pub fn is_parse_failure(stopped_reason: &str) -> bool {
    stopped_reason.starts_with("llm_error") || stopped_reason == "unparseable_reply"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_call_is_sorted_by_the_code_it_ended_with() {
        let k = |s: &str| classify(s);
        assert_eq!(k("ERROR [INVALID_ARGUMENTS] fs.read: missing required argument 'path'"), Some(ErrorKind::InvalidArguments));
        assert_eq!(k("ERROR [NOT_FOUND] fs.read: no such file"), Some(ErrorKind::UnexpectedEnvironment));
        assert_eq!(k("  ERROR [TIMEOUT] term.run: no result within 30000 ms"), Some(ErrorKind::UnexpectedEnvironment));
        assert_eq!(k("ERROR [BACKEND_ERROR] web.read: the page could not be read"), Some(ErrorKind::ProviderError));
        for refused in ["ERROR [PERMISSION_DENIED] fs.write: outside", "ERROR [APPROVAL_DENIED] x", "ERROR [CONFIRMATION_DENIED] browser_click", "ERROR [POLICY_BLOCKED_ORIGIN] local_open_url: no", "ERROR [NEEDS_USER_CHOICE] local_open: ask"] {
            assert_eq!(k(refused), Some(ErrorKind::Refused), "{refused}");
        }
        assert_eq!(k("ERROR [SOMETHING_NEW] x: y"), Some(ErrorKind::Other));
        assert_eq!(k("ERROR something without a code"), Some(ErrorKind::Other));
        assert_eq!(k("fs.read a.txt: the file says ERROR [BACKEND_ERROR] but it is only text"), None, "an error is what the result starts with");
        assert_eq!(k("all fine"), None);
    }

    #[test]
    fn the_tally_is_per_tool_and_per_kind_and_counts_every_call() {
        let calls = [
            ("fs.read", "ERROR [INVALID_ARGUMENTS] fs.read: x"),
            ("fs.read", "ERROR [INVALID_ARGUMENTS] fs.read: y"),
            ("fs.read", "hello"),
            ("web.read", "ERROR [BACKEND_ERROR] web.read: z"),
            ("web.read", "ERROR [TIMEOUT] web.read: slow"),
            ("fs.write", "ERROR [PERMISSION_DENIED] fs.write: no"),
        ];
        let (t, total) = tally(calls);
        assert_eq!(total, 6);
        assert_eq!(t["fs.read"][&ErrorKind::InvalidArguments], 2);
        assert_eq!((t["web.read"][&ErrorKind::ProviderError], t["web.read"][&ErrorKind::UnexpectedEnvironment]), (1, 1));
        assert_eq!(t["fs.write"][&ErrorKind::Refused], 1);
        assert!(!t.contains_key("fs.list"));
        assert!(is_parse_failure("llm_error: the planner was asked more often") && !is_parse_failure("final"));
    }
}
