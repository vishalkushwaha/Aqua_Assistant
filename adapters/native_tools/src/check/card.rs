//! The card the model reads for a check: one line of verdict, the failures as numbered lines, a few lines of source around the first one (card OA4-05, tasks WF-04 and WF-05).
//!
//! ```text
//! check tests: FAILED 2 of 14 (cargo test, 38 s)
//! 1. parse::quotes - src/parse.rs:41: left == right failed: left Some((1, 2)), right Some((1, 3))
//! 2. parse::empty - src/parse.rs:47: assertion failed: parse_pair("").is_some()
//! ```
//!
//! The first line is what the verification gate of the agent reads (`code_verification::check_from_step`): `check <kind>: PASSED|FAILED|NO TESTS RAN|TIMED OUT|RUNNING|NOT RUN`. Nothing a
//! program printed is copied except what a parser took out of it, cut to a line each.

use super::parse::{clip, Counts, Failure, Parsed};
use crate::code::CheckKind;
use serde_json::{json, Value};

/// What came of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Passed,
    Failed,
    /// The runner ran no test: not a pass, whatever its exit code was.
    NoTests,
    TimedOut,
    /// Kept as a background job, not finished.
    Running,
    NotRun,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Passed => "PASSED",
            Status::Failed => "FAILED",
            Status::NoTests => "NO TESTS RAN",
            Status::TimedOut => "TIMED OUT",
            Status::Running => "RUNNING",
            Status::NotRun => "NOT RUN",
        }
    }
}

/// The most failures a card lists.
pub const LISTED: usize = 5;
/// The most characters of one failure line.
pub const LINE_CHARS: usize = 160;
/// The most characters of the verdict and the failure lines together.
pub const CARD_CHARS: usize = 700;
/// The most characters of source around a failure that a card carries, all together.
pub const SNIPPET_CHARS: usize = 400;
/// The lines of the end of the output a card carries when no parser read it.
const TAIL_LINES: usize = 12;
const TAIL_CHARS: usize = 1_200;

/// A few lines of source around a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub file: String,
    pub line: u32,
    /// `(line number, text)`; the failing line is `line`.
    pub lines: Vec<(u32, String)>,
}

impl Snippet {
    pub fn render(&self) -> String {
        let mut out = format!("{}:{}\n", self.file, self.line);
        for (n, text) in &self.lines {
            let mark = if *n == self.line { '>' } else { ' ' };
            out.push_str(&format!("{mark}{n:>4} | {}\n", clip_line(text, 100)));
        }
        out
    }
}

fn clip_line(text: &str, max: usize) -> String {
    let t = text.trim_end().replace('\t', "    ");
    if t.chars().count() <= max {
        t
    } else {
        let mut cut: String = t.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

/// Everything a check came to.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub kind: CheckKind,
    /// The command as one line (`cargo test --offline`).
    pub command: String,
    /// Where it came from (`Cargo.toml`, `package.json scripts.test`).
    pub source: String,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub parsed: Option<Parsed>,
    /// The end of what the command printed, as lines.
    pub tail: String,
    pub offline_failure: bool,
    pub timed_out: bool,
    pub limit_s: u64,
    pub job_id: Option<String>,
    pub not_run: Option<String>,
    pub snippets: Vec<Snippet>,
}

impl Outcome {
    pub fn new(kind: CheckKind, command: String, source: String, cwd: String) -> Outcome {
        Outcome { kind, command, source, cwd, exit_code: None, duration_ms: 0, parsed: None, tail: String::new(), offline_failure: false, timed_out: false, limit_s: 0, job_id: None, not_run: None, snippets: Vec::new() }
    }

    pub fn status(&self) -> Status {
        if self.not_run.is_some() {
            return Status::NotRun;
        }
        if self.job_id.is_some() {
            return Status::Running;
        }
        if self.timed_out {
            return Status::TimedOut;
        }
        let parsed = self.parsed.as_ref();
        let ran_nothing = self.kind == CheckKind::Tests && parsed.and_then(|p| p.ran_any) == Some(false);
        match self.exit_code {
            Some(0) if ran_nothing => Status::NoTests,
            Some(0) if parsed.is_some_and(|p| !p.failures.is_empty() || p.counts.is_some_and(|c| c.failed > 0)) => Status::Failed,
            Some(0) => Status::Passed,
            // A runner that exits with a code of its own when it found nothing (pytest 5, unittest 5) ran no tests.
            Some(_) if ran_nothing && parsed.is_some_and(|p| p.failures.is_empty() && p.failed_count() == 0) => Status::NoTests,
            Some(_) => Status::Failed,
            None => Status::Failed,
        }
    }

    /// The check passed: it ran, found nothing wrong and (for tests) ran at least one test.
    pub fn passed(&self) -> bool {
        self.status() == Status::Passed
    }

    pub fn counts(&self) -> Option<Counts> {
        self.parsed.as_ref().and_then(|p| p.counts)
    }

    fn headline(&self) -> String {
        let status = self.status();
        let seconds = if self.duration_ms >= 950 { format!(", {} s", (self.duration_ms + 500) / 1000) } else { String::new() };
        let what = format!("({}{seconds})", self.command);
        let detail = match status {
            Status::Passed => match (self.counts(), self.kind) {
                (Some(c), CheckKind::Tests) if c.passed > 0 => {
                    let skipped = if c.skipped > 0 { format!(", {} skipped", c.skipped) } else { String::new() };
                    format!(" {} test{}{skipped}", c.passed, if c.passed == 1 { "" } else { "s" })
                }
                _ => String::new(),
            },
            Status::Failed => {
                let parsed = self.parsed.as_ref();
                let failed = parsed.map_or(0, Parsed::failed_count);
                match (self.kind, self.counts()) {
                    (CheckKind::Tests, Some(c)) if failed > 0 && c.passed + c.failed > 0 => format!(" {} of {}", failed.max(c.failed), (c.passed + c.failed).max(failed)),
                    (CheckKind::Tests, _) if failed > 0 => format!(" {failed} test{}", if failed == 1 { "" } else { "s" }),
                    (CheckKind::Lint, _) if failed > 0 => format!(" {failed} finding{}", if failed == 1 { "" } else { "s" }),
                    (_, _) if failed > 0 => format!(" {failed} error{}", if failed == 1 { "" } else { "s" }),
                    _ => match self.exit_code {
                        Some(code) => format!(" exit code {code}"),
                        None => String::new(),
                    },
                }
            }
            Status::NoTests => " the runner ran no test, which is not a pass".to_string(),
            Status::TimedOut => format!(" after {} s", self.limit_s),
            Status::Running => format!(" as {}", self.job_id.as_deref().unwrap_or("a job")),
            Status::NotRun => format!(": {}", self.not_run.as_deref().unwrap_or("")),
        };
        if status == Status::NotRun {
            return format!("check {}: NOT RUN{detail}", self.kind.as_str());
        }
        format!("check {}: {}{detail} {what}", self.kind.as_str(), status.word())
    }

    fn failure_line(n: usize, f: &Failure) -> String {
        let place = match (&f.file, f.line) {
            (Some(file), Some(line)) => format!("{file}:{line}: "),
            (Some(file), None) => format!("{file}: "),
            _ => String::new(),
        };
        let message = if f.message.is_empty() { String::new() } else { f.message.clone() };
        let line = if place.is_empty() && message.is_empty() { format!("{n}. {}", f.id) } else { format!("{n}. {} - {place}{message}", f.id) };
        clip(&line, LINE_CHARS)
    }

    /// The text of the card.
    pub fn text(&self) -> String {
        let mut out = self.headline();
        let status = self.status();
        match status {
            Status::Failed => {
                let failures: &[Failure] = self.parsed.as_ref().map_or(&[], |p| p.failures.as_slice());
                let mut used = out.chars().count();
                let mut shown = 0;
                for (i, f) in failures.iter().take(LISTED).enumerate() {
                    let line = Self::failure_line(i + 1, f);
                    // The verdict and the lines of failures share CARD_CHARS; a line that does not fit is left to the count.
                    if used + line.chars().count() + 1 > CARD_CHARS && shown > 0 {
                        break;
                    }
                    out.push('\n');
                    out.push_str(&line);
                    used += line.chars().count() + 1;
                    shown += 1;
                }
                let total = self.parsed.as_ref().map_or(0, Parsed::failed_count) as usize;
                let more = total.max(failures.len()).saturating_sub(shown);
                if more > 0 {
                    out.push_str(&format!("\n(+{more} more: fix these and run it again, or run one with filter=<name>)"));
                }
                if failures.is_empty() {
                    // Nothing was understood: the end of what the program printed says more than the exit code.
                    if !self.tail.is_empty() {
                        out.push_str("\n--- the last of what it printed ---\n");
                        out.push_str(&tail_of(&self.tail, TAIL_LINES, TAIL_CHARS));
                    }
                }
                if self.offline_failure {
                    out.push_str("\na dependency is not downloaded and this check runs offline: ask the person to allow `cargo fetch`; it was not run online.");
                }
                let mut budget = SNIPPET_CHARS;
                for s in self.snippets.iter().take(1) {
                    let text = s.render();
                    if text.chars().count() <= budget {
                        out.push('\n');
                        out.push_str(&text);
                        budget -= text.chars().count();
                    }
                }
            }
            Status::NoTests => {
                out.push_str("\nCheck the path or the filter; a project with no tests has nothing to run: say so in the answer, do not call it passing.");
            }
            Status::TimedOut => {
                out.push_str("\nIt was stopped. Run it again with background=true (or kind=build, which is kept as a background job) and read it with term.output.");
                if !self.tail.is_empty() {
                    out.push_str("\n--- the last of what it printed ---\n");
                    out.push_str(&tail_of(&self.tail, TAIL_LINES, TAIL_CHARS));
                }
            }
            Status::Running => {
                let id = self.job_id.as_deref().unwrap_or("");
                out.push_str(&format!("\nRead what it prints with term.output job_id={id}; end it with term.stop job_id={id}. It is not a pass until it has ended with exit code 0."));
            }
            Status::Passed | Status::NotRun => {}
        }
        out
    }

    /// The data of the result.
    pub fn data(&self) -> Value {
        let status = self.status();
        let parsed = self.parsed.as_ref();
        let counts = self.counts().map(|c| json!({"passed": c.passed, "failed": c.failed, "skipped": c.skipped}));
        let failures: Vec<Value> = parsed.map(|p| p.failures.iter().take(20).map(|f| json!({"id": f.id, "file": f.file, "line": f.line, "col": f.col, "message": f.message})).collect()).unwrap_or_default();
        let first_error = parsed.and_then(|p| p.failures.first()).map(|f| clip(&format!("{} {}", f.id, f.message), 200));
        let mut data = json!({
            "kind": self.kind.as_str(),
            "state": match status { Status::Running => "running", Status::NotRun => "not_run", Status::TimedOut => "timed_out", _ => "finished" },
            "command": self.command,
            "source": self.source,
            "cwd": self.cwd,
            "exit_code": self.exit_code,
            "duration_ms": self.duration_ms,
            "passed": status == Status::Passed,
            "ran_any": parsed.and_then(|p| p.ran_any),
            "counts": counts,
            "failures": failures,
            "failed_count": parsed.map(Parsed::failed_count),
            "first_error": first_error,
            "offline_failure": self.offline_failure,
            "tool": parsed.map(|p| p.tool.label()),
            "understood": parsed.is_some_and(|p| p.recognised),
        });
        if !self.snippets.is_empty() {
            data["snippets"] = json!(self.snippets.iter().take(3).map(Snippet::render).collect::<Vec<_>>());
        }
        if let Some(id) = &self.job_id {
            data["job_id"] = json!(id);
        }
        if let Some(why) = &self.not_run {
            data["not_run"] = json!(why);
        }
        if !self.tail.is_empty() && status != Status::Passed {
            data["output_tail"] = json!(tail_of(&self.tail, 40, 4_000));
        }
        data
    }
}

/// The last `lines` lines of `text`, at most `chars` characters of them (from the end).
pub fn tail_of(text: &str, lines: usize, chars: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let last = all[all.len().saturating_sub(lines)..].join("\n");
    let n = last.chars().count();
    if n <= chars {
        last
    } else {
        let skip = n - chars;
        let cut: String = last.chars().skip(skip).collect();
        format!("…{cut}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::parse::{Counts, Failure, Parsed, Tool};

    fn failed_tests() -> Outcome {
        let mut p = Parsed::new(Tool::Libtest);
        p.recognised = true;
        p.counts = Some(Counts { passed: 12, failed: 2, skipped: 1 });
        p.ran_any = Some(true);
        for (id, line) in [("parse::quotes", 41), ("parse::empty", 47)] {
            p.failures.push(Failure { id: id.into(), file: Some("src/parse.rs".into()), line: Some(line), col: Some(9), message: "assertion failed".into() });
        }
        let mut o = Outcome::new(CheckKind::Tests, "cargo test --offline".into(), "Cargo.toml".into(), "C:/work/proj".into());
        o.exit_code = Some(101);
        o.duration_ms = 38_200;
        o.parsed = Some(p);
        o
    }

    #[test]
    fn a_failed_test_run_is_a_verdict_and_numbered_lines_inside_the_limit() {
        let o = failed_tests();
        let text = o.text();
        assert!(text.starts_with("check tests: FAILED 2 of 14 (cargo test --offline, 38 s)\n1. parse::quotes - src/parse.rs:41: assertion failed\n2. parse::empty - src/parse.rs:47"), "{text}");
        assert!(text.chars().count() <= CARD_CHARS, "{} chars", text.chars().count());
        assert_eq!(o.status(), Status::Failed);
        assert!(!o.passed());
        let data = o.data();
        assert_eq!((data["passed"].as_bool(), data["failed_count"].as_u64(), data["counts"]["passed"].as_u64()), (Some(false), Some(2), Some(12)));
        assert_eq!(data["failures"][0]["file"], "src/parse.rs");
        assert!(data["first_error"].as_str().unwrap().starts_with("parse::quotes"));
    }

    #[test]
    fn a_pass_a_run_with_no_tests_and_a_timeout_are_three_different_verdicts() {
        let mut o = failed_tests();
        o.exit_code = Some(0);
        o.parsed.as_mut().unwrap().failures.clear();
        o.parsed.as_mut().unwrap().counts = Some(Counts { passed: 14, failed: 0, skipped: 2 });
        assert!(o.text().starts_with("check tests: PASSED 14 tests, 2 skipped (cargo test --offline, 38 s)"), "{}", o.text());
        assert!(o.passed());
        // The runner exits 0 and says it ran nothing: not a pass.
        o.parsed.as_mut().unwrap().counts = Some(Counts::default());
        o.parsed.as_mut().unwrap().ran_any = Some(false);
        assert_eq!(o.status(), Status::NoTests);
        assert!(o.text().starts_with("check tests: NO TESTS RAN"), "{}", o.text());
        assert!(!o.passed() && o.data()["passed"] == false);
        // pytest and unittest exit 5 when they collect nothing: also no tests, not a failure.
        o.exit_code = Some(5);
        assert_eq!(o.status(), Status::NoTests);
        // A build that ran nothing is not "no tests".
        let mut b = Outcome::new(CheckKind::Build, "cargo build".into(), "Cargo.toml".into(), "x".into());
        b.exit_code = Some(0);
        assert!(b.passed() && b.text().starts_with("check build: PASSED (cargo build)"), "{}", b.text());
        // Timed out.
        let mut t = Outcome::new(CheckKind::Tests, "cargo test".into(), "Cargo.toml".into(), "x".into());
        t.timed_out = true;
        t.limit_s = 1;
        t.tail = "line one\nline two".into();
        assert!(t.text().starts_with("check tests: TIMED OUT after 1 s (cargo test)") && t.text().contains("line two"), "{}", t.text());
        assert_eq!(t.data()["state"], "timed_out");
    }

    #[test]
    fn a_failure_no_parser_understood_shows_the_end_of_the_output_and_the_exit_code() {
        let mut o = Outcome::new(CheckKind::Build, "make".into(), "Makefile".into(), "x".into());
        o.exit_code = Some(2);
        o.tail = (1..=30).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let text = o.text();
        assert!(text.starts_with("check build: FAILED exit code 2 (make)"), "{text}");
        assert!(text.contains("line 30") && text.contains("line 19") && !text.contains("line 18\n"), "{text}");
    }

    #[test]
    fn more_failures_than_a_card_lists_are_counted_and_a_running_job_says_how_to_read_it() {
        let mut o = failed_tests();
        let p = o.parsed.as_mut().unwrap();
        for i in 0..9 {
            p.failures.push(Failure { id: format!("t{i}"), file: None, line: None, col: None, message: String::new() });
        }
        p.counts = Some(Counts { passed: 3, failed: 11, skipped: 0 });
        let text = o.text();
        assert!(text.contains("FAILED 11 of 14") && text.contains("(+6 more"), "{text}");
        let mut j = Outcome::new(CheckKind::Build, "cargo build --offline".into(), "Cargo.toml".into(), "x".into());
        j.job_id = Some("job_7".into());
        assert!(j.text().contains("RUNNING as job_7") && j.text().contains("term.output job_id=job_7"), "{}", j.text());
        assert_eq!(j.data()["job_id"], "job_7");
        let mut n = Outcome::new(CheckKind::Tests, "cargo test".into(), "Cargo.toml".into(), "x".into());
        n.not_run = Some("the folder holds no project".into());
        assert_eq!(n.text(), "check tests: NOT RUN: the folder holds no project");
    }

    #[test]
    fn a_snippet_is_drawn_with_its_failing_line_marked() {
        let s = Snippet { file: "src/a.rs".into(), line: 3, lines: vec![(2, "let a = 1;".into()), (3, "    assert_eq!(a, 2);".into()), (4, "}".into())] };
        assert_eq!(s.render(), "src/a.rs:3\n    2 | let a = 1;\n>   3 |     assert_eq!(a, 2);\n    4 | }\n");
    }

    #[test]
    fn the_tail_keeps_the_last_lines_and_the_last_characters() {
        assert_eq!(tail_of("a\nb\nc\nd", 2, 100), "c\nd");
        assert_eq!(tail_of("abcdef", 5, 3), "…def");
        assert_eq!(tail_of("", 5, 10), "");
    }
}
