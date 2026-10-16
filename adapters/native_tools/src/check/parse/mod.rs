//! Failure parsers: the output of a test, build, lint or type-check command as a few lines instead of a log (card OA4-05, task WF-05 of the OpenCode audit).
//!
//! One function per tool, all pure: they read text and never run anything, never read a file and never panic, whatever the text is (a cut, a binary, a missing newline). What they
//! return is [`Parsed`]: the counts the tool printed, the failures it named (a test id, a file and line, a message) and whether it ran anything at all. The card that the model reads
//! is made from it by [`super::card`]. A failure of one tool that the parser does not understand is not lost: the card then shows the end of what the program printed.
//!
//! Tools: `libtest` (`cargo test`), `rustc` (`cargo build|check|clippy`), `pytest`, `unittest`, `node_test` (`node --test`), `jest` (jest and vitest), `tsc`, `eslint`, `ruff`, `gotest`.

pub mod eslint;
pub mod gotest;
pub mod jest;
pub mod libtest;
pub mod node_test;
pub mod pytest;
pub mod ruff;
pub mod rustc;
pub mod tsc;
pub mod unittest;

use serde::Serialize;

/// The tools whose output is understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Libtest,
    Rustc,
    Pytest,
    Unittest,
    NodeTest,
    Jest,
    Tsc,
    Eslint,
    Ruff,
    GoTest,
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Tool::Libtest => "libtest",
            Tool::Rustc => "rustc",
            Tool::Pytest => "pytest",
            Tool::Unittest => "unittest",
            Tool::NodeTest => "node --test",
            Tool::Jest => "jest",
            Tool::Tsc => "tsc",
            Tool::Eslint => "eslint",
            Tool::Ruff => "ruff",
            Tool::GoTest => "go test",
        }
    }
}

/// What a tool counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

impl Counts {
    pub fn total(&self) -> u32 {
        self.passed + self.failed + self.skipped
    }
}

/// One thing that failed: a test, a compile error, a lint finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// The id the tool gives it: a test name, an error code (`E0308`, `TS2322`, `F401`), `error`.
    pub id: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub message: String,
}

/// What a parser made of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub tool: Tool,
    /// The tool's own summary (or at least one of its lines) was found: the text is the output of this tool.
    pub recognised: bool,
    pub counts: Option<Counts>,
    pub failures: Vec<Failure>,
    /// How many errors the tool says there are (it can be more than the failures that were found).
    pub error_total: Option<u32>,
    /// `Some(false)`: the tool says it ran nothing (no tests collected, `running 0 tests`).
    pub ran_any: Option<bool>,
}

impl Parsed {
    pub fn new(tool: Tool) -> Parsed {
        Parsed { tool, recognised: false, counts: None, failures: Vec::new(), error_total: None, ran_any: None }
    }

    /// The number of things that failed: what the tool says, or what was found, whichever is more.
    pub fn failed_count(&self) -> u32 {
        let counted = self.counts.map_or(0, |c| c.failed);
        counted.max(self.error_total.unwrap_or(0)).max(self.failures.len() as u32)
    }
}

/// The most failures a parser keeps; the count says how many there were.
pub const MAX_FAILURES: usize = 50;
/// The most characters of a message that a parser keeps.
pub const MAX_MESSAGE: usize = 300;

/// Compile a pattern once. The patterns are constants, and a test runs every parser over every golden, so a pattern that does not compile is found there.
macro_rules! re {
    ($pattern:expr) => {{
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new($pattern).expect("a constant pattern"))
    }};
}
pub(crate) use re;

/// The text a tool printed, as plain lines: colours (`ESC [ ... letter`) and carriage returns are taken out.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // A control sequence: `ESC [` parameters, then a letter; an `ESC` on its own is dropped.
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(n) = chars.next() {
                    if n.is_ascii_alphabetic() || n == '~' {
                        break;
                    }
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

/// At most `max` characters, with `…` where it was cut, on one line.
pub fn clip(text: &str, max: usize) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        return one;
    }
    let mut cut: String = one.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// A path as the model should read it: forward slashes.
pub fn slash(path: &str) -> String {
    path.trim().replace('\\', "/")
}

pub(crate) fn num(s: &str) -> Option<u32> {
    s.trim().parse().ok()
}

pub(crate) fn push_failure(p: &mut Parsed, f: Failure) {
    if p.failures.len() < MAX_FAILURES && !p.failures.iter().any(|g| g == &f) {
        p.failures.push(f);
    }
}

/// The program the failing output came from, read from the command that ran: `cargo test` is libtest, `python -m pytest` is pytest.
pub fn tool_for(exe: &str, args: &[String]) -> Option<Tool> {
    let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_ascii_lowercase();
    let name = name.trim_end_matches(".exe").trim_end_matches(".cmd");
    let first = args.iter().map(|a| a.to_ascii_lowercase()).find(|a| !a.starts_with('-') && !a.starts_with('+'));
    match name {
        "cargo" => match first.as_deref() {
            Some("test" | "nextest") => Some(Tool::Libtest),
            Some("build" | "check" | "clippy" | "b" | "c") => Some(Tool::Rustc),
            _ => None,
        },
        "pytest" | "py.test" => Some(Tool::Pytest),
        "python" | "python3" | "py" => {
            let m = args.windows(2).find(|w| w[0] == "-m").map(|w| w[1].to_ascii_lowercase());
            match m.as_deref() {
                Some("pytest") => Some(Tool::Pytest),
                Some("unittest") => Some(Tool::Unittest),
                Some("ruff") => Some(Tool::Ruff),
                _ => None,
            }
        }
        "node" if args.iter().any(|a| a == "--test") => Some(Tool::NodeTest),
        "jest" | "vitest" => Some(Tool::Jest),
        "tsc" => Some(Tool::Tsc),
        "eslint" => Some(Tool::Eslint),
        "ruff" => Some(Tool::Ruff),
        "go" => match first.as_deref() {
            Some("test" | "build" | "vet") => Some(Tool::GoTest),
            _ => None,
        },
        _ => None,
    }
}

/// Which tool printed this text, when it can be told from the text: what a package manager's script prints (`npm test`, `npm run lint`) has no name of its own.
pub fn sniff(text: &str) -> Option<Tool> {
    let has = |p: &str| text.contains(p);
    if has("test result:") && (has("running ") || has("failures:")) {
        return Some(Tool::Libtest);
    }
    if has("\nℹ tests ") || text.starts_with("ℹ tests ") || has("\n# tests ") {
        return Some(Tool::NodeTest);
    }
    if re!(r"(?m)^\s*(Tests|Test Suites|Test Files):\s").is_match(text) || re!(r"(?m)^\s+Tests\s+\d+ ").is_match(text) || has("No tests found, exiting") {
        return Some(Tool::Jest);
    }
    if re!(r"(?m)^Ran \d+ tests? in ").is_match(text) {
        return Some(Tool::Unittest);
    }
    if re!(r"(?m) in [\d.]+s\b").is_match(text) && (has("passed") || has("failed") || has("no tests ran")) {
        return Some(Tool::Pytest);
    }
    if re!(r"(?m)^.+\(\d+,\d+\): error TS\d+: ").is_match(text) || re!(r"(?m)^.+:\d+:\d+ - error TS\d+: ").is_match(text) {
        return Some(Tool::Tsc);
    }
    if has("✖ ") && re!(r"(?m)^\s+\d+:\d+\s+(error|warning)\s").is_match(text) {
        return Some(Tool::Eslint);
    }
    if re!(r"(?m)^\s*\S+:\d+:\d+: [A-Z]+\d+ ").is_match(text) || has("All checks passed!") || re!(r"(?m)^Found \d+ errors?\.").is_match(text) {
        return Some(Tool::Ruff);
    }
    if re!(r"(?m)^(ok|FAIL|\?)\s+\S+\s").is_match(text) || has("--- FAIL:") {
        return Some(Tool::GoTest);
    }
    if re!(r"(?m)^error(\[\w+\])?: ").is_match(text) || re!(r"(?m)^.+:\d+:\d+: error").is_match(text) {
        return Some(Tool::Rustc);
    }
    None
}

/// Read `stdout` and `stderr` of a command with the parser of `tool`. `exit` is the exit code (a parser may use it where a tool prints nothing on success: `tsc`, `eslint`).
/// When the text is not the tool's (`hint` was wrong or there is none), the text is looked at to find whose it is.
pub fn parse(tool: Tool, stdout: &str, stderr: &str, exit: i32) -> Parsed {
    let text = format!("{}\n{}", clean(stdout), clean(stderr));
    let first = parse_text(tool, &text, exit);
    if first.recognised {
        return first;
    }
    match sniff(&text) {
        Some(other) if other != tool => {
            let second = parse_text(other, &text, exit);
            if second.recognised {
                second
            } else {
                first
            }
        }
        _ => first,
    }
}

/// [`parse`] for a command whose tool is not known from its name: the text says.
pub fn parse_sniffed(stdout: &str, stderr: &str, exit: i32) -> Option<Parsed> {
    let text = format!("{}\n{}", clean(stdout), clean(stderr));
    let tool = sniff(&text)?;
    let p = parse_text(tool, &text, exit);
    p.recognised.then_some(p)
}

fn parse_text(tool: Tool, text: &str, exit: i32) -> Parsed {
    match tool {
        Tool::Libtest => libtest::parse(text),
        Tool::Rustc => rustc::parse(text),
        Tool::Pytest => pytest::parse(text),
        Tool::Unittest => unittest::parse(text),
        Tool::NodeTest => node_test::parse(text),
        Tool::Jest => jest::parse(text),
        Tool::Tsc => tsc::parse(text, exit),
        Tool::Eslint => eslint::parse(text, exit),
        Tool::Ruff => ruff::parse(text, exit),
        Tool::GoTest => gotest::parse(text),
    }
}

/// A `Regex` capture as a number, 0 where it is not there.
pub(crate) fn cap_num(c: &regex::Captures, i: usize) -> u32 {
    c.get(i).and_then(|m| num(m.as_str())).unwrap_or(0)
}
