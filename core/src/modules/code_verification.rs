//! Evidence for coding tasks: did the tests, build, type check or linter actually pass?
//!
//! "The implementation is complete" is the model talking. What counts is a command that was run and
//! exited 0: the tests, the build, the type checker, the linter. This module recognises those commands
//! among the agent's calls and reads their exit codes. It only reads what happened; it never runs anything.
//!
//! Verification gate v2 (card OA4-04; tasks WF-07, TM-03 and ED-15 of the OpenCode audit, and A6-43 of the sixth audit):
//!
//! * a check is read wherever it ran: a `term.run`, a `check.run`, each program of a `term.shell` line, and a background job (`term.start`, a `term.run` that was kept as a job, a
//!   `check.run` with `background`) once its `term.output` says it ended;
//! * a command that runs nothing (`--no-run`, `--help`, `--collect-only`, `--list`, `--dry-run`) is no check, a runner that ran no test is not a pass, `npm run <script> --if-present` that
//!   found no script is not a check, and a check that ran in a folder none of the changed files is in is not evidence of that change;
//! * what a check proves is matched with what the goal asked for ([`GoalNeeds`]): `cargo check` is not "the tests pass";
//! * a change is anything that can alter what a check saw: a file tool, a program that writes code (`cargo fmt`, `sed -i`, `npm install`, `git checkout` ...), a git tool that moves the
//!   tree. A commit changes no file and makes no check old.

use crate::memory::experience::model::{Verification, VerificationMethod};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    Tests,
    Build,
    TypeCheck,
    Lint,
}

impl CheckKind {
    pub fn method(&self) -> VerificationMethod {
        match self {
            Self::Tests => VerificationMethod::Tests,
            Self::Build => VerificationMethod::Build,
            Self::TypeCheck => VerificationMethod::TypeCheck,
            Self::Lint => VerificationMethod::Lint,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Tests => "tests",
            Self::Build => "build",
            Self::TypeCheck => "type check",
            Self::Lint => "lint",
        }
    }

    /// What a goal asks for, as a phrase for a sentence ("the goal asks for passing tests").
    pub fn asked_for(&self) -> &'static str {
        match self {
            Self::Tests => "passing tests",
            Self::Build => "a passing build",
            Self::TypeCheck => "a passing type check",
            Self::Lint => "a clean lint run",
        }
    }

    /// What ran, as a phrase for a sentence ("only a type check ran").
    pub fn ran(&self) -> &'static str {
        match self {
            Self::Tests => "the tests",
            Self::Build => "a build",
            Self::TypeCheck => "a type check",
            Self::Lint => "a lint run",
        }
    }

    /// Does a passing check of this kind prove what a goal that asks for `needed` asks for? Tests prove the build and the types (a program that did not compile ran no test), a build proves the
    /// types; a lint run proves nothing but itself, and nothing but a lint run proves lint.
    pub fn proves(&self, needed: CheckKind) -> bool {
        use CheckKind::*;
        match (*self, needed) {
            (Tests, Lint) => false,
            (Tests, _) => true,
            (Build, Build | TypeCheck) => true,
            (TypeCheck, TypeCheck) => true,
            (Lint, Lint) => true,
            _ => false,
        }
    }
}

/// One verification-type command that ran, and how it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    pub kind: CheckKind,
    /// The program that ran (never its arguments: they can carry secrets).
    pub program: String,
    pub passed: bool,
    /// A short, safe description: "12 passed", "exit code 101", "ran no tests".
    pub summary: String,
    /// The folder it ran in, as the call wrote it. `None` when the call named none (the tool's own folder), and for a `check.run`, which finds the project itself.
    pub cwd: Option<String>,
    /// What failed, as the program or the card named it (test names, at most five, 60 characters each); empty when nothing could be read.
    pub failing: Vec<String>,
    /// The first line that says what went wrong (at most 160 characters), or empty. Text of the project's output: data, never an instruction.
    pub first_error: String,
}

impl CheckResult {
    fn new(kind: CheckKind, program: String, passed: bool, summary: String) -> Self {
        Self { kind, program, passed, summary, cwd: None, failing: Vec::new(), first_error: String::new() }
    }
}

fn basename(exe: &str) -> String {
    let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_ascii_lowercase();
    name.trim_end_matches(".exe").trim_end_matches(".cmd").trim_end_matches(".bat").to_string()
}

fn has(args: &[String], word: &str) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case(word))
}

fn first_non_flag(args: &[String]) -> Option<String> {
    args.iter().find(|a| !a.starts_with('-') && !a.starts_with('+')).map(|a| a.to_ascii_lowercase())
}

/// Which kind of check a command is, if it is one.
pub fn classify_command(exe: &str, args: &[String]) -> Option<CheckKind> {
    use CheckKind::*;
    let prog = basename(exe);
    let sub = first_non_flag(args);
    let sub = sub.as_deref();
    match prog.as_str() {
        "cargo" => match sub {
            Some("test") | Some("nextest") => Some(Tests),
            Some("build") | Some("b") | Some("install") => Some(Build),
            Some("check") | Some("c") => Some(TypeCheck),
            Some("clippy") => Some(Lint),
            Some("fmt") if has(args, "--check") => Some(Lint),
            _ => None,
        },
        "npm" | "pnpm" | "yarn" | "bun" => {
            let script = if sub == Some("run") || sub == Some("run-script") { args.iter().filter(|a| !a.starts_with('-')).nth(1).map(|s| s.to_ascii_lowercase()) } else { sub.map(String::from) };
            match script.as_deref() {
                Some("test") | Some("t") | Some("test:unit") | Some("test:e2e") | Some("e2e") => Some(Tests),
                Some("build") | Some("compile") => Some(Build),
                Some("lint") | Some("eslint") => Some(Lint),
                Some("typecheck") | Some("type-check") | Some("tsc") => Some(TypeCheck),
                _ => None,
            }
        }
        "npx" | "pnpx" | "bunx" => match sub {
            Some("tsc") => Some(TypeCheck),
            Some("eslint") | Some("prettier") if sub == Some("eslint") || has(args, "--check") => Some(Lint),
            Some("vitest") | Some("jest") | Some("mocha") | Some("playwright") => Some(Tests),
            _ => None,
        },
        "tsc" => Some(TypeCheck),
        "eslint" | "ruff" | "flake8" | "pylint" | "golangci-lint" | "rubocop" | "shellcheck" => Some(Lint),
        "mypy" | "pyright" => Some(TypeCheck),
        "pytest" | "py.test" | "vitest" | "jest" | "mocha" | "rspec" | "phpunit" => Some(Tests),
        "python" | "python3" | "py" => {
            let m = args.windows(2).find(|w| w[0] == "-m").map(|w| w[1].to_ascii_lowercase());
            match m.as_deref() {
                Some("pytest") | Some("unittest") | Some("nose2") => Some(Tests),
                // A file that compiles is the weakest type check there is, and it is the one a Python edit is given automatically (card OA4-04, ED-15).
                Some("mypy") | Some("pyright") | Some("py_compile") | Some("compileall") => Some(TypeCheck),
                Some("ruff") | Some("flake8") | Some("pylint") => Some(Lint),
                _ => None,
            }
        }
        "go" => match sub {
            Some("test") => Some(Tests),
            Some("build") | Some("install") => Some(Build),
            Some("vet") => Some(Lint),
            _ => None,
        },
        "dotnet" => match sub {
            Some("test") => Some(Tests),
            Some("build") | Some("publish") => Some(Build),
            _ => None,
        },
        "mvn" | "mvnw" | "gradle" | "gradlew" => match sub {
            Some("test") | Some("verify") | Some("check") => Some(Tests),
            Some("package") | Some("build") | Some("compile") | Some("assemble") | Some("install") => Some(Build),
            _ => None,
        },
        "make" | "ninja" | "msbuild" => match sub {
            Some("test") | Some("check") => Some(Tests),
            None | Some("all") | Some("build") => Some(Build),
            _ => None,
        },
        "cmake" if has(args, "--build") => Some(Build),
        _ => None,
    }
}

/// The exit code on the first line of a `term.run` result ("exit code 0"), if the command ran.
pub fn exit_code_of(observation: &str) -> Option<i32> {
    // The tool host wraps what a command printed in an `<untrusted:…>` line; the code comes right after it.
    let first = observation.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("<untrusted"))?;
    first.strip_prefix("exit code ")?.trim().parse().ok()
}

/// A test runner that ran nothing proves nothing, even with exit code 0.
fn ran_no_tests(observation: &str) -> bool {
    let o = observation.to_ascii_lowercase();
    // cargo prints one "test result" line per target (unit tests, integration tests, doc-tests, ...)
    // and an empty target says "running 0 tests". The run ran tests if any target did.
    if o.contains("test result:") {
        return !o.lines().any(|l| l.contains("test result:") && !l.contains(" 0 passed"));
    }
    [
        "running 0 tests",
        "0 tests run",
        "collected 0 items",
        "no tests found",
        "no tests ran",
        "no test files",
        "no tests to run",
        "0 passing",
        "tests: 0 total",
        "ran 0 tests",
    ]
    .iter()
    .any(|p| o.contains(p))
}

/// A short description of how it ended, from its own output. Counts only; no file names or paths.
fn summarize(observation: &str, exit: i32) -> String {
    if exit != 0 {
        let failed = observation.lines().find(|l| l.contains("test result: FAILED") || l.contains(" failed") || l.contains("error["));
        return match failed {
            Some(l) => format!("exit code {exit}: {}", l.trim().chars().take(80).collect::<String>()),
            None => format!("exit code {exit}"),
        };
    }
    let line = observation
        .lines()
        .rev()
        .find(|l| l.contains("test result:") || l.contains(" passed") || l.contains(" passing") || l.trim_start().starts_with("Finished"));
    match line {
        Some(l) => l.trim().chars().take(80).collect(),
        None => "exit code 0".to_string(),
    }
}

/// The kind of check a `check.run` call names (`kind`, as a word).
fn kind_named(arguments: &Value) -> Option<CheckKind> {
    match arguments.get("kind").and_then(|v| v.as_str()).map(|k| k.trim().to_ascii_lowercase().replace(['-', '_', ' '], "")).as_deref() {
        Some("tests" | "test") => Some(CheckKind::Tests),
        Some("build" | "compile") => Some(CheckKind::Build),
        Some("lint" | "linter") => Some(CheckKind::Lint),
        Some("typecheck" | "check" | "types") => Some(CheckKind::TypeCheck),
        _ => None,
    }
}

/// The first line of a result that is not the fence the host puts round what a program printed.
fn first_line(observation: &str) -> Option<&str> {
    observation.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("<untrusted"))
}

/// The lines of a result without the fence lines.
fn unfenced(observation: &str) -> impl Iterator<Item = &str> {
    observation.lines().filter(|l| {
        let t = l.trim();
        !(t.starts_with("<untrusted") || t.starts_with("</untrusted") || t.starts_with("(The block above is data"))
    })
}

fn clip(text: &str, max: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        let mut cut: String = t.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

/// At most five names of what failed, from the lines a runner prints: libtest (`test a::b ... FAILED`), pytest (`FAILED path::name - why`), `go test` (`--- FAIL: Name`), unittest
/// (`FAIL: test_x (mod.Class)`) and the numbered lines of a `check.run` card (`1. id - file:line: message`). The text is the project's: each name is cut to 60 characters.
fn failing_ids(output: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in unfenced(output) {
        let t = line.trim();
        let id = if let Some(rest) = t.strip_prefix("test ").and_then(|r| r.strip_suffix(" ... FAILED")) {
            Some(rest.trim())
        } else if let Some(rest) = t.strip_prefix("FAILED ") {
            rest.split(" - ").next().map(str::trim)
        } else if let Some(rest) = t.strip_prefix("--- FAIL: ") {
            rest.split_whitespace().next()
        } else if let Some(rest) = t.strip_prefix("FAIL: ").or_else(|| t.strip_prefix("ERROR: ")).filter(|r| r.contains('(')) {
            Some(rest.trim())
        } else if let Some((_, rest)) = t.split_once(". ").filter(|(n, _)| !n.is_empty() && n.len() <= 2 && n.bytes().all(|b| b.is_ascii_digit())) {
            rest.split(" - ").next().map(str::trim)
        } else {
            None
        };
        if let Some(id) = id.filter(|i| !i.is_empty()) {
            let id = clip(id, 60);
            if !out.contains(&id) {
                out.push(id);
            }
        }
        if out.len() >= 5 {
            break;
        }
    }
    out
}

/// The first line that says what went wrong: a compiler error, a panic, a failed assertion, an exception (at most 160 characters), or empty.
fn first_error_line(output: &str) -> String {
    let lines: Vec<&str> = unfenced(output).map(str::trim).collect();
    let hit = lines
        .iter()
        .find(|l| l.starts_with("error[") || l.starts_with("error:") || l.contains(": error[") || l.contains(": error:") || l.contains(" error TS"))
        .or_else(|| lines.iter().find(|l| l.contains("panicked at") || l.starts_with("AssertionError") || l.starts_with("E   ") || l.starts_with("Error:") || l.starts_with("FAILED ")))
        .or_else(|| lines.iter().find(|l| l.starts_with("1. ")));
    hit.map(|l| clip(l, 160)).unwrap_or_default()
}

/// A `check.run` step (card OA4-05) read as a check. The card it answers with starts `check <kind>: PASSED|FAILED|NO TESTS RAN|TIMED OUT|RUNNING|NOT RUN ...`: the first three are evidence, the
/// others are not (a run that did not end, or did not start, shows nothing either way, as a `term.run` that timed out does not). The verdict is the tool's own, made from the exit code and from what
/// the runner counted, so a run that ran no test is `NO TESTS RAN` and not a pass.
fn check_from_check_run(arguments: &Value, observation: &str) -> Option<CheckResult> {
    let kind = kind_named(arguments)?;
    let first = first_line(observation)?;
    let rest = first.strip_prefix("check ")?;
    let (_, verdict) = rest.split_once(": ")?;
    // `cargo test --offline, 38 s` inside the last parentheses: the program is its first word.
    let program = verdict.rsplit_once('(').and_then(|(_, inner)| inner.split([' ', ',']).next()).filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))).unwrap_or("check.run").to_ascii_lowercase();
    let program = program.trim_end_matches(".exe").to_string();
    let summary = |from: &str| verdict.strip_prefix(from).unwrap_or(verdict).trim().chars().take(80).collect::<String>();
    let mut result = if verdict.starts_with("PASSED") {
        CheckResult::new(kind, program, true, summary("PASSED"))
    } else if verdict.starts_with("NO TESTS RAN") {
        CheckResult::new(kind, program, false, "the test runner ran no tests".into())
    } else if verdict.starts_with("FAILED") {
        CheckResult::new(kind, program, false, summary("FAILED"))
    } else {
        return None;
    };
    if !result.passed {
        result.failing = failing_ids(observation);
        result.first_error = observation.lines().map(str::trim).find(|l| l.starts_with("1. ")).map(|l| clip(l.trim_start_matches("1. "), 160)).unwrap_or_default();
    }
    Some(result)
}

/// The most diagnostics of a failed check that the model is shown (card OA4-04, ED-15).
pub const SHOWN_DIAGNOSTICS: usize = 8;

/// A line that is a compiler's complaint: cargo (`src/lib.rs:4:5: error[E0308]: mismatched types` with `--message-format short`, or `error[E0308]: ...` of the long form), tsc
/// (`src/a.ts(3,5): error TS2322: ...`), and the like.
fn is_diagnostic(line: &str) -> bool {
    let l = line.trim();
    l.starts_with("error[") || (l.starts_with("error:") && !l.starts_with("error: could not compile")) || l.contains(": error[") || l.contains(": error:") || l.contains(": error TS") || l.contains(") error TS")
}

/// The output of the check that follows an edit, reduced to what the model needs (card OA4-04, ED-15): the exit code, at most [`SHOWN_DIAGNOSTICS`] of the compiler's complaints and how many
/// more there were; for a Python file that does not compile, the last lines of the traceback. The fence the host puts round what a program printed stays round what is left. `None` when the
/// text is not a failed check that this can read (or would not be shorter): the whole text is shown then, as it always was.
pub fn summarise_check_output(observation: &str) -> Option<String> {
    let code = exit_code_of(observation)?;
    if code == 0 {
        return None;
    }
    let lines: Vec<&str> = observation.lines().collect();
    let open = lines.iter().position(|l| l.trim_start().starts_with("<untrusted"));
    let close = lines.iter().rposition(|l| l.trim_start().starts_with("</untrusted"));
    let (head, inner, tail): (&[&str], &[&str], &[&str]) = match (open, close) {
        (Some(o), Some(c)) if o < c => (&lines[..=o], &lines[o + 1..c], &lines[c..]),
        _ => (&[], &lines[..], &[]),
    };
    let diagnostics: Vec<&str> = inner.iter().copied().filter(|l| is_diagnostic(l)).collect();
    let mut kept: Vec<String> = vec![format!("exit code {code}")];
    if !diagnostics.is_empty() {
        kept.extend(diagnostics.iter().take(SHOWN_DIAGNOSTICS).map(|l| clip(l, 200)));
        if diagnostics.len() > SHOWN_DIAGNOSTICS {
            kept.push(format!("(+{} more)", diagnostics.len() - SHOWN_DIAGNOSTICS));
        }
    } else if inner.iter().any(|l| l.trim_start().starts_with("Traceback") || l.contains("SyntaxError") || l.contains("IndentationError") || l.contains("TabError")) {
        let printed: Vec<&str> = inner.iter().copied().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("exit code ") && !l.trim_start().starts_with("--- std")).collect();
        kept.extend(printed.iter().rev().take(4).rev().map(|l| clip(l, 200)));
    } else {
        return None;
    }
    let mut out: Vec<String> = head.iter().map(|l| l.to_string()).collect();
    out.extend(kept);
    out.extend(tail.iter().map(|l| l.to_string()));
    let text = out.join("\n");
    (text.len() < observation.len()).then_some(text)
}

/// Arguments that make a command run nothing: it asks for its help, its version, a list of what it would run, or a dry run. It exits 0 and proves nothing (audit A6-43).
const RUNS_NOTHING: &[&str] = &["--no-run", "--help", "-h", "--version", "--list", "--list-tests", "--listtests", "--collect-only", "--co", "--dry-run", "--just-print", "--recon"];

/// Does this command, as written, run none of the code it names?
pub fn runs_nothing(exe: &str, args: &[String]) -> bool {
    let prog = basename(exe);
    args.iter().any(|a| RUNS_NOTHING.iter().any(|w| a.eq_ignore_ascii_case(w)))
        || (matches!(prog.as_str(), "make" | "ninja") && args.iter().any(|a| a == "-n"))
        || (prog == "cargo" && has(args, "nextest") && has(args, "list"))
}

/// `npm run typecheck --if-present` exits 0 and does nothing when the project has no such script. npm echoes the script's command (a line that starts with `>`) when it runs one.
fn vacuous_if_present(exe: &str, args: &[String], output: &str) -> bool {
    basename(exe) == "npm" && has(args, "--if-present") && !unfenced(output).any(|l| l.trim_start().starts_with('>'))
}

/// A program that changes the files of the project when it is run: a formatter that writes, a fixer, an installer, an in-place edit, a version-control command that moves the tree, a file
/// command. A check that passed before one of these is out of date (audit A6-43: `cargo fmt`, `sed -i` and `git checkout` did not make a check stale). A shell or an interpreter running a
/// script is not listed: what it does cannot be read from its arguments.
pub fn command_writes_code(exe: &str, args: &[String]) -> bool {
    let prog = basename(exe);
    let sub = first_non_flag(args);
    let sub = sub.as_deref();
    let flag = |f: &str| has(args, f);
    match prog.as_str() {
        "cargo" => match sub {
            Some("fmt") => !flag("--check"),
            Some("fix" | "add" | "remove" | "update" | "generate-lockfile") => true,
            Some("clippy") => flag("--fix"),
            _ => false,
        },
        "npm" | "pnpm" | "yarn" | "bun" => {
            let script = if matches!(sub, Some("run" | "run-script")) { args.iter().filter(|a| !a.starts_with('-')).nth(1).map(|s| s.to_ascii_lowercase()) } else { None };
            matches!(sub, Some("install" | "i" | "add" | "remove" | "rm" | "uninstall" | "update" | "upgrade" | "ci" | "dedupe")) || matches!(script.as_deref(), Some("format" | "fmt" | "fix" | "lint:fix" | "prettier"))
        }
        "npx" | "pnpx" | "bunx" => (matches!(sub, Some("prettier")) && flag("--write")) || (matches!(sub, Some("eslint")) && flag("--fix")),
        "prettier" => flag("--write"),
        "eslint" => flag("--fix"),
        "ruff" => flag("--fix") || (sub == Some("format") && !flag("--check") && !flag("--diff")),
        "black" | "isort" | "autopep8" | "yapf" | "rustfmt" => !flag("--check") && !flag("--diff"),
        "gofmt" => flag("-w"),
        "go" => matches!(sub, Some("fmt" | "get" | "generate")) || (sub == Some("mod") && args.iter().any(|a| a.eq_ignore_ascii_case("tidy"))),
        "clang-format" => flag("-i"),
        "dotnet" => (sub == Some("format") && !flag("--verify-no-changes")) || matches!(sub, Some("add" | "remove" | "restore")),
        "pip" | "pip3" | "uv" | "poetry" => matches!(sub, Some("install" | "uninstall" | "add" | "remove" | "sync" | "update")),
        "sed" => args.iter().any(|a| a == "-i" || a.starts_with("-i") || a == "--in-place" || a.starts_with("--in-place=")),
        "perl" => args.iter().any(|a| a == "-pi" || a == "-ni" || a.starts_with("-i") || a.starts_with("-pi")),
        "git" => matches!(sub, Some("apply" | "am" | "checkout" | "restore" | "reset" | "stash" | "merge" | "rebase" | "pull" | "switch" | "cherry-pick" | "revert" | "clean" | "mv" | "rm")),
        "rm" | "del" | "erase" | "rmdir" | "rd" | "mv" | "move" | "ren" | "rename" | "cp" | "copy" | "xcopy" | "robocopy" | "touch" | "tee" => true,
        "python" | "python3" | "py" => {
            let m = args.windows(2).find(|w| w[0] == "-m").map(|w| w[1].to_ascii_lowercase());
            match m.as_deref() {
                Some("black" | "isort" | "autopep8" | "yapf") => !flag("--check") && !flag("--diff"),
                Some("pip") => args.iter().any(|a| matches!(a.as_str(), "install" | "uninstall")),
                Some("ruff") => flag("--fix") || (args.iter().any(|a| a == "format") && !flag("--check")),
                _ => false,
            }
        }
        _ => false,
    }
}

fn args_of(arguments: &Value) -> Vec<String> {
    arguments.get("args").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// One command that ran as a check, read from its exit code and what it printed. `None` when it is no check, ran nothing (see [`runs_nothing`]) or did nothing (see `vacuous_if_present`).
fn read_command(exe: &str, args: &[String], cwd: Option<&str>, exit: i32, output: &str) -> Option<CheckResult> {
    let kind = classify_command(exe, args)?;
    if runs_nothing(exe, args) || vacuous_if_present(exe, args, output) {
        return None;
    }
    let program = basename(exe);
    let mut result = if exit == 0 && kind == CheckKind::Tests && ran_no_tests(output) {
        CheckResult::new(kind, program, false, "the test runner ran no tests".into())
    } else {
        CheckResult::new(kind, program, exit == 0, summarize(output, exit))
    };
    result.cwd = cwd.map(str::trim).filter(|c| !c.is_empty()).map(String::from);
    if !result.passed {
        result.failing = failing_ids(output);
        result.first_error = first_error_line(output);
    }
    Some(result)
}

/// The programs a `term.shell` call ran, in order, as (program, arguments): the line is split as the tool splits it. Empty when the line cannot be read.
fn shell_programs(arguments: &Value) -> Vec<(String, Vec<String>)> {
    arguments.get("command").and_then(|v| v.as_str()).and_then(|c| aqua_native_tools::term::rules::shell_segments(c).ok()).unwrap_or_default()
}

/// What each program of a `term.shell` result printed, with its exit code, in order: the text of one program starts with its own `exit code N` line, and the next one follows it. A result
/// that was cut, or that ended in an error, holds the programs that finished (the error repeats what the ones before it wrote).
fn shell_chunks(observation: &str) -> Vec<(i32, String)> {
    let mut chunks: Vec<(i32, String)> = Vec::new();
    for line in unfenced(observation) {
        if let Some(code) = line.trim().strip_prefix("exit code ").and_then(|c| c.trim().parse::<i32>().ok()) {
            chunks.push((code, String::new()));
        } else if let Some((_, text)) = chunks.last_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    chunks
}

struct ShellPart {
    writes: bool,
    check: Option<CheckResult>,
}

/// What each program of a `term.shell` call that ran came to: whether it wrote code, and the check it was, in the order of the line.
fn shell_parts(arguments: &Value, observation: &str) -> Vec<ShellPart> {
    let programs = shell_programs(arguments);
    let chunks = shell_chunks(observation);
    let cwd = arguments.get("cwd").and_then(|v| v.as_str());
    programs
        .iter()
        .zip(chunks.iter())
        .map(|((exe, args), (exit, output))| ShellPart { writes: command_writes_code(exe, args), check: read_command(exe, args, cwd, *exit, output) })
        .collect()
}

/// Read one tool step as a check, if it is one. `tool` is the tool name, `arguments` what it was called with. A `term.shell` line is read as its last program that is a check. A background
/// job is read by [`evidence_from`], which sees the steps that started it and the one that read it.
pub fn check_from_step(tool: &str, arguments: &Value, observation: &str) -> Option<CheckResult> {
    match tool {
        "check.run" => check_from_check_run(arguments, observation),
        "term.run" => {
            let exe = arguments.get("exe").and_then(|v| v.as_str())?;
            let args = args_of(arguments);
            classify_command(exe, &args)?;
            // A command that did not run (refused, timed out, not found) is not evidence either way.
            let exit = exit_code_of(observation)?;
            read_command(exe, &args, arguments.get("cwd").and_then(|v| v.as_str()), exit, observation)
        }
        "term.shell" => shell_parts(arguments, observation).into_iter().rev().find_map(|p| p.check),
        _ => None,
    }
}

/// Tools that change the code. A check that passed before one of these is out of date: the file tools that write, move, copy, delete or restore, and the git tools that move the working
/// tree. A commit changes no file, so it makes no check old.
pub fn is_code_change(tool: &str) -> bool {
    if let Some(rest) = tool.strip_prefix("fs.") {
        return ["write", "edit", "patch", "append", "create", "delete", "move", "copy", "replace", "mkdir", "remove", "restore", "undo"].iter().any(|w| rest.contains(w));
    }
    matches!(
        tool,
        "git.apply" | "git.merge" | "git.reset" | "git.discard" | "git.switch" | "git.stash" | "git.pull" | "git.checkout" | "git.restore" | "git.rebase" | "git.cherry_pick" | "git.revert" | "git.clean" | "git.mv" | "git.rm" | "git.resolve_conflict"
    )
}

/// The paths a file tool's call names, as the call wrote them (for the rule about the folder a check ran in).
fn changed_paths_of(tool: &str, arguments: &Value) -> Vec<String> {
    let one = |k: &str| arguments.get(k).and_then(|v| v.as_str()).filter(|p| !p.is_empty()).map(String::from);
    match tool {
        "fs.edit" | "fs.edit_lines" | "fs.write" | "fs.patch" => crate::modules::coding_session::paths_of(tool, arguments).into_iter().filter(|p| !p.is_empty()).collect(),
        "fs.delete" | "fs.restore" => one("path").into_iter().collect(),
        "fs.move" => one("src").into_iter().chain(one("dst")).collect(),
        "fs.copy" => one("dst").into_iter().collect(),
        _ => Vec::new(),
    }
}

/// What the goal asks to be proved (card OA4-04, WF-07): the words of the goal say which kind of check settles it. A goal that names none (`fix the bug in parse`) is settled by any passing check,
/// as it always was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoalNeeds {
    pub tests: bool,
    pub build: bool,
    pub typecheck: bool,
    pub lint: bool,
}

impl GoalNeeds {
    pub fn from_goal(goal: &str) -> Self {
        let g = goal.to_ascii_lowercase();
        let words: Vec<&str> = g.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
        let has_word = |ws: &[&str]| ws.iter().any(|w| words.contains(w));
        Self {
            tests: has_word(&["test", "tests", "testing", "pytest", "unittest", "unittests", "jest", "vitest", "mocha"]),
            build: has_word(&["build", "builds"]),
            // A goal that is to make the code compile is settled by a check that compiles it: `cargo check` is that, as a build and the tests are.
            typecheck: has_word(&["typecheck", "typechecks", "typechecker", "tsc", "mypy", "pyright", "compile", "compiles", "compiling", "compilation"]) || g.contains("type error") || g.contains("type check") || g.contains("type-check"),
            lint: has_word(&["lint", "linter", "linting", "clippy", "eslint", "ruff", "flake8", "pylint"]),
        }
    }

    /// The kinds the goal asks for, strongest first.
    pub fn wanted(&self) -> Vec<CheckKind> {
        let mut out = Vec::new();
        if self.tests {
            out.push(CheckKind::Tests);
        }
        if self.build {
            out.push(CheckKind::Build);
        }
        if self.typecheck {
            out.push(CheckKind::TypeCheck);
        }
        if self.lint {
            out.push(CheckKind::Lint);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        !(self.tests || self.build || self.typecheck || self.lint)
    }
}

/// What the run's checks say about the outcome. `steps` are (tool, arguments, observation) in order.
#[derive(Debug, Clone, PartialEq)]
pub enum CodeEvidence {
    /// No test, build, type check or lint command ran (or none produced a usable result).
    None,
    /// The last check passed and nothing changed after it.
    Passed(CheckResult),
    /// The last check failed.
    Failed(CheckResult),
    /// The last check passed, but the code was changed afterwards and the check was not run again.
    Stale(CheckResult),
    /// Checks ran and none of them is the kind the goal asked for (`cargo check` for "make the tests pass"): `wanted` is the kind that is missing, `got` the kind of the last check.
    WrongKind { wanted: CheckKind, got: CheckKind },
}

/// One thing that happened in the run that a check cares about.
#[derive(Debug, Clone)]
enum Ev {
    /// Code was changed; the paths that are known (none for a command that changes files without naming them).
    Change { paths: Vec<String> },
    /// A check ended. `after` is the first event that is later than the code the check saw: the one after the check, or, for a background job, the one after it was started.
    Check { result: CheckResult, after: usize },
}

/// A background job that may turn out to be a check.
struct Job {
    exe: String,
    args: Vec<String>,
    cwd: Option<String>,
    /// The kind and program when the job was started by `check.run` (which knows them), instead of by a command that is classified.
    named: Option<(CheckKind, String)>,
    started_at: usize,
    output: String,
    reported: bool,
}

impl Job {
    fn started(exe: &str, args: Vec<String>, cwd: Option<String>, started_at: usize) -> Self {
        Self { exe: exe.to_string(), args, cwd, named: None, started_at, output: String::new(), reported: false }
    }
}

/// The job id in a line: `job_3`.
fn job_token(text: &str) -> Option<String> {
    let at = text.find("job_")?;
    let id: String = text[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    (id.len() > 4).then_some(id)
}

/// The first line of a `term.output` result: `job job_3 is exited (exit code 0)`. The id, the state, and the exit code when it has one.
fn job_state(observation: &str) -> Option<(String, String, Option<i32>)> {
    let first = first_line(observation)?;
    let rest = first.strip_prefix("job ")?;
    let (id, tail) = rest.split_once(" is ")?;
    let state: String = tail.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    let code = tail.split_once("(exit code ").and_then(|(_, c)| c.split([')', ',']).next()).and_then(|c| c.trim().parse().ok());
    Some((id.trim().to_string(), state, code))
}

/// The events of a run, in order.
fn events(steps: &[(String, Value, String)]) -> Vec<Ev> {
    let mut evs: Vec<Ev> = Vec::new();
    let mut jobs: HashMap<String, Job> = HashMap::new();
    for (tool, arguments, observation) in steps {
        let failed = host_call_failed(observation);
        let exe = arguments.get("exe").and_then(|v| v.as_str()).unwrap_or("");
        let args = args_of(arguments);
        let cwd = arguments.get("cwd").and_then(|v| v.as_str()).map(String::from);
        match tool.as_str() {
            "check.run" => {
                if let Some(result) = check_from_check_run(arguments, observation) {
                    let after = evs.len() + 1;
                    evs.push(Ev::Check { result, after });
                } else if let (Some(kind), Some(first)) = (kind_named(arguments), first_line(observation)) {
                    // `check build: RUNNING as job_3 (cargo build --offline)`: the job is the check.
                    if let (true, Some(id)) = (first.contains(": RUNNING as "), job_token(first)) {
                        let program = first.rsplit_once('(').and_then(|(_, inner)| inner.split([' ', ',', ')']).next()).unwrap_or("check.run").to_ascii_lowercase();
                        let mut job = Job::started("", Vec::new(), None, evs.len());
                        job.named = Some((kind, program));
                        jobs.insert(id, job);
                    }
                }
            }
            "term.run" => {
                let kept = first_line(observation).filter(|l| l.starts_with("job_") && l.contains(" is still running") && l.contains("background job")).and_then(job_token);
                if command_writes_code(exe, &args) {
                    if !failed {
                        evs.push(Ev::Change { paths: Vec::new() });
                    }
                } else if let Some(id) = kept {
                    jobs.insert(id, Job::started(exe, args, cwd, evs.len()));
                } else if let Some(result) = check_from_step(tool, arguments, observation) {
                    let after = evs.len() + 1;
                    evs.push(Ev::Check { result, after });
                }
            }
            "term.shell" => {
                let programs = shell_programs(arguments);
                let parts = shell_parts(arguments, observation);
                // A program that wrote code, but whose result is not here (the result was cut, or ended in an error), may still have written: it counts as a change.
                for (at, (exe, argv)) in programs.iter().enumerate() {
                    match parts.get(at) {
                        Some(part) => {
                            if part.writes {
                                evs.push(Ev::Change { paths: Vec::new() });
                            } else if let Some(result) = part.check.clone() {
                                let after = evs.len() + 1;
                                evs.push(Ev::Check { result, after });
                            }
                        }
                        None if command_writes_code(exe, argv) && !failed => evs.push(Ev::Change { paths: Vec::new() }),
                        None => {}
                    }
                }
            }
            "term.start" => {
                if command_writes_code(exe, &args) {
                    evs.push(Ev::Change { paths: Vec::new() });
                } else if let Some(id) = first_line(observation).and_then(|l| l.strip_prefix("Started ")).and_then(job_token) {
                    jobs.insert(id, Job::started(exe, args, cwd, evs.len()));
                }
            }
            "term.output" => {
                let (Some((id, state, code)), false) = (job_state(observation), failed) else { continue };
                let Some(job) = jobs.get_mut(&id) else { continue };
                if job.output.len() < 200_000 {
                    job.output.push_str(&unfenced(observation).skip_while(|l| !l.trim_start().starts_with("job ")).skip(1).collect::<Vec<_>>().join("\n"));
                    job.output.push('\n');
                }
                // Only a job that ended on its own with an exit code is a check: one that was stopped or ran out of time proves nothing, and one still running is not over.
                if state != "exited" || job.reported {
                    continue;
                }
                let Some(exit) = code else { continue };
                let result = match &job.named {
                    Some((kind, program)) => {
                        let mut r = if exit == 0 && *kind == CheckKind::Tests && ran_no_tests(&job.output) {
                            CheckResult::new(*kind, program.clone(), false, "the test runner ran no tests".into())
                        } else {
                            CheckResult::new(*kind, program.clone(), exit == 0, summarize(&job.output, exit))
                        };
                        if !r.passed {
                            r.failing = failing_ids(&job.output);
                            r.first_error = first_error_line(&job.output);
                        }
                        Some(r)
                    }
                    None => read_command(&job.exe, &job.args, job.cwd.as_deref(), exit, &job.output),
                };
                job.reported = true;
                if let Some(result) = result {
                    evs.push(Ev::Check { result, after: job.started_at });
                }
            }
            other => {
                if is_code_change(other) && !failed {
                    evs.push(Ev::Change { paths: changed_paths_of(other, arguments) });
                }
            }
        }
    }
    evs
}

/// A place as one comparable string: where the file tools resolve it, in lower case with `/`.
fn place_of(path: &str) -> String {
    crate::modules::coding_session::canonical(path)
}

fn is_absolute_place(place: &str) -> bool {
    place.starts_with('/') || place.as_bytes().get(1) == Some(&b':')
}

fn is_under(child: &str, parent: &str) -> bool {
    let parent = parent.trim_end_matches('/');
    child == parent || child.strip_prefix(parent).is_some_and(|rest| rest.starts_with('/'))
}

/// Did this check run where the code it is meant to prove was changed? A check that ran in a folder that holds none of the files the run changed before it proves nothing about them (audit A6-43:
/// "the working folder is ignored"). Only decided when both are known places: a check with no folder, a folder or a file that is not an absolute place is let through.
fn ran_where_it_changed(evs: &[Ev], result: &CheckResult, before: usize) -> bool {
    let Some(cwd) = result.cwd.as_deref().map(place_of).filter(|c| is_absolute_place(c)) else { return true };
    let changed: Vec<String> = evs[..before.min(evs.len())]
        .iter()
        .flat_map(|e| match e {
            Ev::Change { paths } => paths.iter().map(|p| place_of(p)).filter(|p| is_absolute_place(p)).collect::<Vec<_>>(),
            Ev::Check { .. } => Vec::new(),
        })
        .collect();
    changed.is_empty() || changed.iter().any(|p| is_under(p, &cwd))
}

/// The evidence of a run whose goal names no kind of check: the last check decides.
pub fn evidence_from(steps: &[(String, Value, String)]) -> CodeEvidence {
    evidence_for(steps, GoalNeeds::default())
}

/// The evidence of a run for a goal: the checks have to be of the kind the goal asks for ([`GoalNeeds`]).
pub fn evidence_for_goal(steps: &[(String, Value, String)], goal: &str) -> CodeEvidence {
    evidence_for(steps, GoalNeeds::from_goal(goal))
}

pub fn evidence_for(steps: &[(String, Value, String)], needs: GoalNeeds) -> CodeEvidence {
    let evs = events(steps);
    // The checks that count, with the place after which a change makes each one old.
    let checks: Vec<(usize, &CheckResult, usize)> = evs
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Ev::Check { result, after } if ran_where_it_changed(&evs, result, *after) => Some((i, result, *after)),
            _ => None,
        })
        .collect();
    let Some(&(_, last, last_after)) = checks.last() else { return CodeEvidence::None };
    let changed_after = |after: usize| evs[after.min(evs.len())..].iter().any(|e| matches!(e, Ev::Change { .. }));
    let wanted = needs.wanted();
    if wanted.is_empty() {
        return if !last.passed {
            CodeEvidence::Failed(last.clone())
        } else if changed_after(last_after) {
            CodeEvidence::Stale(last.clone())
        } else {
            CodeEvidence::Passed(last.clone())
        };
    }
    let (mut failed, mut stale, mut wrong): (Option<&CheckResult>, Option<&CheckResult>, Option<CheckKind>) = (None, None, None);
    let mut ok: Vec<(usize, &CheckResult)> = Vec::new();
    for need in wanted {
        match checks.iter().rev().find(|(_, c, _)| c.kind.proves(need)) {
            None => {
                wrong.get_or_insert(need);
            }
            Some(&(i, c, after)) => {
                if !c.passed {
                    failed.get_or_insert(c);
                } else if changed_after(after) {
                    stale.get_or_insert(c);
                } else {
                    ok.push((i, c));
                }
            }
        }
    }
    if let Some(c) = failed {
        CodeEvidence::Failed(c.clone())
    } else if let Some(c) = stale {
        CodeEvidence::Stale(c.clone())
    } else if let Some(wanted) = wrong {
        CodeEvidence::WrongKind { wanted, got: last.kind }
    } else {
        CodeEvidence::Passed(ok.into_iter().max_by_key(|(i, _)| *i).map(|(_, c)| c.clone()).unwrap_or_else(|| last.clone()))
    }
}

/// One check of a run, as the coding record shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckSeen {
    pub result: CheckResult,
    /// The code was changed after the check began (so what it said may no longer be so).
    pub stale: bool,
}

/// The checks of a run that count, oldest first: the ones [`evidence_for`] reads, with whether the code changed after each.
pub fn checks_seen(steps: &[(String, Value, String)]) -> Vec<CheckSeen> {
    let evs = events(steps);
    evs.iter()
        .filter_map(|e| match e {
            Ev::Check { result, after } if ran_where_it_changed(&evs, result, *after) => Some(CheckSeen { result: result.clone(), stale: evs[(*after).min(evs.len())..].iter().any(|e| matches!(e, Ev::Change { .. })) }),
            _ => None,
        })
        .collect()
}

impl CodeEvidence {
    /// As a verification, or `None` when this evidence says nothing.
    pub fn verification(&self) -> Option<Verification> {
        match self {
            Self::None => None,
            Self::Passed(c) => Some(Verification::new(true, c.kind.method(), format!("{} {} passed: {}", c.program, c.kind.label(), c.summary))),
            Self::Failed(c) => Some(Verification::new(false, c.kind.method(), format!("{} {} failed: {}", c.program, c.kind.label(), c.summary))),
            // Unknown, not a failure: the code changed after the check and nothing shows it still passes.
            Self::Stale(c) => Some(Verification::new(false, VerificationMethod::ModelClaim, format!("the {} passed, but the code was changed afterwards and it was not run again", c.kind.label()))),
            // Unknown, not a failure: what ran is not what the goal asked for.
            Self::WrongKind { wanted, got } => Some(Verification::new(false, VerificationMethod::ModelClaim, format!("the goal asks for {}, but only {} ran", wanted.asked_for(), got.ran()))),
        }
    }

    /// One sentence about what the run's checks showed, written by the program from the evidence and never by the model, for the end of a coding run's answer (`AQUA_ANSWER_EVIDENCE`).
    /// The name of the program and the short line the check itself ended with (at most 80 characters: "12 passed", "exit code 101: ..."), nothing else of what the project printed.
    pub fn answer_note(&self) -> String {
        match self {
            Self::None => "(Aqua saw no test, build, type check or lint run in this task, so this is not confirmed.)".to_string(),
            Self::Passed(c) => format!("(Aqua read the run: {} {} passed: {}.)", c.program, c.kind.label(), c.summary),
            Self::Failed(c) => format!("(Aqua read the run: the last {} ({}) failed: {}. This is not confirmed.)", c.kind.label(), c.program, c.summary),
            Self::Stale(c) => format!("(Aqua read the run: the {} passed, but the code was changed after it and it was not run again. This is not confirmed.)", c.kind.label()),
            Self::WrongKind { wanted, got } => format!("(Aqua read the run: the task asks for {}, but only {} ran. This is not confirmed.)", wanted.asked_for(), got.ran()),
        }
    }
}

/// Is this the name of a file of source code (not a document or a setting)? A change to one of these is a change to code. A view of the one table of languages
/// (`aqua_native_tools::code::lang`, card OA4-05, CU-06).
pub(crate) fn path_is_code(path: &str) -> bool {
    aqua_native_tools::code::is_source_path(path)
}

/// Has this run, by what it did, turned into a coding task? Yes once it has changed a source file (`fs.edit`, `fs.write`, `fs.patch` on
/// code) or run a test, build or lint command. The goal's words are only a guess made before anything happened ("make the page load
/// faster" turns out to be a change to `app.ts`); what the run did is what it was. `steps` are (tool, arguments, observation).
pub fn run_is_coding(steps: &[(String, Value, String)]) -> bool {
    steps.iter().any(|(tool, args, _)| match tool.as_str() {
        "fs.edit" | "fs.edit_lines" | "fs.write" => args.get("path").and_then(|p| p.as_str()).is_some_and(path_is_code),
        "fs.patch" => {
            let edits_touch_code = args.get("edits").and_then(|e| e.as_array()).is_some_and(|e| e.iter().any(|x| x.get("path").and_then(|p| p.as_str()).is_some_and(path_is_code)));
            let diff_touches_code = args.get("diff").and_then(|d| d.as_str()).is_some_and(|d| d.lines().filter_map(|l| l.strip_prefix("+++ ")).any(|p| path_is_code(p.split('\t').next().unwrap_or(""))));
            edits_touch_code || diff_touches_code
        }
        "term.run" | "term.start" => {
            let exe = args.get("exe").and_then(|v| v.as_str()).unwrap_or("");
            classify_command(exe, &args_of(args)).is_some()
        }
        // A line of programs, one of which is a check (card OA4-04, TM-03).
        "term.shell" => shell_programs(args).iter().any(|(exe, argv)| classify_command(exe, argv).is_some()),
        // The tool that runs a project's own check (card OA4-05): a run that called it is a coding run.
        "check.run" => true,
        _ => false,
    })
}

/// Did a tool of the tool host fail? Its failures start `ERROR [CODE] tool:`. What the tool *returned* is not looked at: a file that says
/// "could not" in a comment is not a failed read. A command that ran and exited non-zero did not fail as a *call*: it ran, and its exit code is
/// the result (the checks above read it as evidence), so it stays an ordinary result for the planner to read.
pub fn host_call_failed(observation: &str) -> bool {
    observation.trim_start().starts_with("ERROR")
}

/// The check to run after a source edit. Docs and unknown types have none. The program and arguments are fixed; the caller supplies the working directory.
///
/// A view of the one table of languages (`aqua_native_tools::code::lang`, card OA4-05, CU-06): what a language's tests are is said there and nowhere else.
pub fn check_command_for(path: &str) -> Option<(&'static str, &'static [&'static str])> {
    aqua_native_tools::code::language_of(path)?.test_command()
}

/// How the line of [`check_hint_for`] starts. The later hint of `coding_session::annotate` looks for it, so that an edit result carries one next-step hint and not two (card OA5-01).
pub const CHECK_HINT_PREFIX: &str = "Check: run ";

/// One line telling the model which check to run after a source edit. Documents and unknown types add nothing.
pub fn check_hint_for(path: &str) -> Option<String> {
    let (exe, args) = check_command_for(path)?;
    Some(format!("{CHECK_HINT_PREFIX}{exe} {} and read the exit code before you say this is fixed.", args.join(" ")))
}

/// Hints for the files a successful write, edit or patch names. Empty when the tool did not change source.
pub fn check_hints(tool: &str, data: &Value) -> String {
    if !matches!(tool, "fs.write" | "fs.edit" | "fs.edit_lines" | "fs.patch") {
        return String::new();
    }
    if data.get("kind").and_then(|v| v.as_str()) == Some("dir") {
        return String::new();
    }
    let mut paths: Vec<&str> = Vec::new();
    if let Some(p) = data.get("path").and_then(|v| v.as_str()) {
        paths.push(p);
    }
    if let Some(files) = data.get("files").and_then(|v| v.as_array()) {
        for f in files {
            if f.get("changed").and_then(|v| v.as_bool()) == Some(false) {
                continue;
            }
            if let Some(p) = f.get("path").and_then(|v| v.as_str()) {
                paths.push(p);
            }
        }
    }
    let lines: Vec<String> = paths.into_iter().filter_map(check_hint_for).collect();
    if lines.is_empty() { String::new() } else { format!("\n{}", lines.join("\n")) }
}

/// Coding tasks, from the goal's words.
pub fn goal_is_coding(goal: &str) -> bool {
    let g = goal.to_ascii_lowercase();
    let words: Vec<&str> = g.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has_word = |ws: &[&str]| ws.iter().any(|w| words.contains(w));
    has_word(&["refactor", "compile", "compiles", "cargo", "npm", "pytest", "typecheck", "linter", "lint", "bug", "bugs", "unit", "regression"])
        || g.contains("failing test")
        || g.contains("fix the test")
        || g.contains("make the tests")
        || g.contains("tests pass")
        || g.contains("build the project")
        || g.contains("fix the build")
        || g.contains("type error")
        || g.contains("implement a function")
        || g.contains("add a function")
        || g.contains("write a function")
}

#[cfg(test)]
mod run_tests {
    use super::*;
    use serde_json::json;

    fn step(tool: &str, args: Value) -> (String, Value, String) {
        (tool.into(), args, String::new())
    }

    #[test]
    fn a_run_is_a_coding_run_once_it_changes_source_code_or_runs_a_check_whatever_the_goal_said() {
        assert!(run_is_coding(&[step("fs.read", json!({"path": "app.ts"})), step("fs.edit", json!({"path": "src/app.ts", "old": "a", "new": "b"}))]));
        assert!(run_is_coding(&[step("fs.write", json!({"path": "main.rs", "content": "fn main() {}"}))]));
        assert!(run_is_coding(&[step("fs.patch", json!({"edits": [{"path": "notes.txt", "old": "a", "new": "b"}, {"path": "lib/util.py", "old": "a", "new": "b"}]}))]));
        assert!(run_is_coding(&[step("fs.patch", json!({"diff": "--- a/src/x.go\n+++ b/src/x.go\n@@ -1 +1 @@\n-a\n+b\n"}))]));
        assert!(run_is_coding(&[step("term.run", json!({"exe": "cargo", "args": ["test"]}))]));
        assert!(run_is_coding(&[step("term.run", json!({"exe": "npm", "args": ["run", "build"]}))]));
        // Not coding: notes, documents, settings, reading, and commands that are not checks.
        assert!(!run_is_coding(&[step("fs.edit", json!({"path": "notes.txt", "old": "a", "new": "b"})), step("fs.write", json!({"path": "README.md", "content": "x"}))]));
        assert!(!run_is_coding(&[step("fs.patch", json!({"edits": [{"path": "settings.json", "old": "a", "new": "b"}]}))]));
        assert!(!run_is_coding(&[step("fs.read", json!({"path": "app.ts"})), step("fs.grep", json!({"pattern": "x"})), step("term.run", json!({"exe": "hostname"})), step("fs.delete", json!({"path": "old.rs"}))]));
        assert!(!run_is_coding(&[]));
    }

    #[test]
    fn a_host_tool_has_failed_only_when_it_says_so_and_not_because_of_words_in_what_it_returned() {
        assert!(host_call_failed("ERROR [NOT_FOUND] fs.read: no such file"));
        assert!(host_call_failed("  ERROR [PERMISSION_DENIED] fs.write: (POLICY) refused"));
        // A command that ran is a result, whatever its exit code: the check evidence reads the code.
        assert!(!host_call_failed("<untrusted:abc>\nexit code 101\nerror[E0308]: mismatched types"));
        assert!(!host_call_failed("<untrusted:abc>\nexit code 0\ntest result: ok. 3 passed; 0 failed"));
        // A read that returns a file whose text says "not found" or "could not" worked.
        assert!(!host_call_failed("<untrusted:abc>\n1\t// the file could not be found: handled below\n2\tif not found { return }"));
        assert!(!host_call_failed("Wrote 20 bytes to a.txt."));
        assert!(!host_call_failed(""));
    }

    #[test]
    fn a_source_file_names_its_check_and_a_document_does_not() {
        assert_eq!(check_command_for("crates/app/src/lib.rs"), Some(("cargo", &["test", "--offline"][..])));
        assert_eq!(check_command_for(r"C:\work\app\src\main.py"), Some(("python", &["-m", "pytest"][..])));
        assert_eq!(check_command_for("web/src/App.tsx"), Some(("npm", &["test"][..])));
        assert_eq!(check_command_for("documentation/README.md"), None);
    }

    #[test]
    fn a_source_edit_names_its_check_and_a_folder_or_a_read_does_not() {
        let hint = check_hints("fs.edit", &json!({"path": "src/lib.rs"}));
        assert!(hint.contains("cargo test --offline"), "{hint}");
        assert!(check_hints("fs.write", &json!({"path": "notes", "kind": "dir"})).is_empty());
        assert!(check_hints("fs.read", &json!({"path": "src/lib.rs"})).is_empty());
        let patch = check_hints("fs.patch", &json!({"files": [{"path": "a.md", "changed": true}, {"path": "web/App.tsx", "changed": true}]}));
        assert!(patch.contains("npm test") && !patch.contains("cargo"), "{patch}");
    }

    #[test]
    fn a_source_file_names_a_language_server_and_a_document_does_not() {
        let language_server_for = |path: &str| aqua_native_tools::code::language_of(path).and_then(|language| language.language_server());
        assert_eq!(language_server_for("src/lib.rs"), Some("rust-analyzer"));
        assert_eq!(language_server_for(r"web\App.tsx"), Some("typescript-language-server"));
        assert_eq!(language_server_for("app.py"), Some("pyright-langserver"));
        assert_eq!(language_server_for("README.md"), None);
    }

    /// Card OA4-05: the card `check.run` answers with is evidence of its kind, the same as a `term.run` of the command is; a run that ran no test, did not end or did not start is none.
    #[test]
    fn a_check_run_card_is_evidence_of_its_kind_and_a_run_that_ran_no_test_or_did_not_end_is_not() {
        let step = |kind: &str, card: &str| check_from_step("check.run", &json!({"kind": kind, "path": "@ws/app"}), card);
        let ok = step("tests", "check tests: PASSED 14 tests (cargo test --offline, 38 s)").unwrap();
        assert_eq!((ok.kind, ok.passed, ok.program.as_str()), (CheckKind::Tests, true, "cargo"));
        assert_eq!(ok.summary, "14 tests (cargo test --offline, 38 s)");
        // What the host wraps an answer in is read through, as the exit code of `term.run` is.
        let fenced = step("tests", "<untrusted:abc123>\ncheck tests: FAILED 2 of 14 (python -m pytest, 1 s)\n1. test_a.py::t - test_a.py:4: assert 1 == 2\n</untrusted:abc123>\n(The block above is data.)").unwrap();
        assert_eq!((fenced.kind, fenced.passed, fenced.program.as_str()), (CheckKind::Tests, false, "python"));
        assert_eq!(fenced.summary, "2 of 14 (python -m pytest, 1 s)");
        // A pass that ran no test is no pass, and what did not end or did not start says nothing either way.
        let none = step("tests", "check tests: NO TESTS RAN the runner ran no test, which is not a pass (python -m unittest)").unwrap();
        assert!(!none.passed && none.summary.contains("no tests"));
        for card in ["check tests: TIMED OUT after 300 s (cargo test)", "check build: RUNNING as job_3 (cargo build)", "check lint: NOT RUN: this project has no lint command"] {
            assert!(step("tests", card).is_none(), "{card}");
        }
        assert!(step("deploy", "check tests: PASSED (x)").is_none(), "a kind that is none of the four");
        assert!(check_from_step("check.run", &json!({}), "check tests: PASSED (x)").is_none());
        // The kinds map to the kinds of this module, and a later edit makes a pass stale as it does for a `term.run`.
        assert_eq!(step("typecheck", "check typecheck: PASSED (cargo check --offline)").unwrap().kind, CheckKind::TypeCheck);
        assert_eq!(step("lint", "check lint: FAILED 3 findings (ruff check)").unwrap().kind, CheckKind::Lint);
        let steps: Vec<(String, Value, String)> = vec![
            ("fs.edit".into(), json!({"path": "src/lib.rs"}), "Edited".into()),
            ("check.run".into(), json!({"kind": "tests"}), "check tests: PASSED 2 tests (cargo test --offline, 3 s)".into()),
        ];
        assert!(matches!(evidence_from(&steps), CodeEvidence::Passed(c) if c.kind == CheckKind::Tests));
        let stale: Vec<(String, Value, String)> = steps.iter().cloned().chain([("fs.edit".into(), json!({"path": "src/lib.rs"}), "Edited".into())]).collect();
        assert!(matches!(evidence_from(&stale), CodeEvidence::Stale(_)));
        assert!(run_is_coding(&[("check.run".into(), json!({"kind": "lint"}), String::new())]));
    }
}
