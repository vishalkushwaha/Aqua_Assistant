//! The errors a check printed after an edit, as a short block per file (card OA6-03, task CU-10 of the OpenCode audit: "the practical LSP value").
//!
//! OpenCode appends the language server's errors to the result of an edit. Aqua has no language server; it has the check that follows an edit (`python -m py_compile`, and with
//! `AQUA_CODING_CHECK=on` `cargo check` and `npm run typecheck`), and the compiler says the same things. This module reads what such a check printed and makes of it
//!
//! ```text
//! <diagnostics file="src/x.rs">
//! ERROR [12:5] mismatched types (E0308)
//! </diagnostics>
//! ```
//!
//! Errors only (a warning is left out: it is noise at 900 tokens), at most [`MAX_PER_FILE`] for a file and [`BUDGET_TOKENS`] in all, the files the run changed first. Every function is pure:
//! it reads text, runs nothing and reads no file, so what a program printed (which holds text from the project and so is data, never instructions) is only ever cut and quoted. The
//! caller keeps the host's fence (`<untrusted:...>`) round what this makes. The failure parsers of `check::parse` (card OA4-05) do the reading for cargo and tsc; the Python,
//! Go and .NET forms are read here.

use super::parse::{self, clean, clip, re, slash, Failure};

/// The most errors shown for one file.
pub const MAX_PER_FILE: usize = 20;
/// The most tokens (four characters each, by the estimate used elsewhere) the block takes in all.
pub const BUDGET_TOKENS: usize = 900;
/// The longest message, in characters.
pub const MAX_MESSAGE: usize = 300;

/// One error: where it is (when the compiler said), its code (`E0308`, `TS2322`, `CS1002`, `SyntaxError`) and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub code: String,
    pub message: String,
}

impl Diag {
    fn from_failure(f: Failure) -> Diag {
        Diag { file: f.file, line: f.line, col: f.col, code: if f.id == "error" { String::new() } else { f.id }, message: f.message }
    }

    /// `ERROR [12:5] message (E0308)`: the place when it is known, and the code after the message.
    fn text(&self) -> String {
        let place = match (self.line, self.col) {
            (Some(l), Some(c)) => format!(" [{l}:{c}]"),
            (Some(l), None) => format!(" [{l}]"),
            _ => String::new(),
        };
        let code = if self.code.is_empty() { String::new() } else { format!(" ({})", self.code) };
        format!("ERROR{place} {}{code}", self.message)
    }
}

/// How many tokens a text is, by the estimate the program uses (four characters to a token, rounded up).
pub fn tokens_of(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Rustc,
    Tsc,
    PyCompile,
    Go,
    DotNet,
    /// A package manager's script (`npm run typecheck`) or a program this does not know: the text says.
    Unknown,
}

fn program_name(exe: &str) -> String {
    let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_ascii_lowercase();
    name.trim_end_matches(".exe").trim_end_matches(".cmd").to_string()
}

fn source_of(exe: &str, args: &[String]) -> Source {
    let first = args.iter().map(|a| a.to_ascii_lowercase()).find(|a| !a.starts_with('-') && !a.starts_with('+'));
    match program_name(exe).as_str() {
        "cargo" => match first.as_deref() {
            Some("check" | "build" | "clippy" | "test" | "c" | "b" | "run") => Source::Rustc,
            _ => Source::Unknown,
        },
        "rustc" => Source::Rustc,
        "tsc" => Source::Tsc,
        "python" | "python3" | "py" if args.iter().any(|a| a == "py_compile") => Source::PyCompile,
        "go" => match first.as_deref() {
            Some("build" | "vet" | "install" | "run") => Source::Go,
            _ => Source::Unknown,
        },
        "dotnet" | "msbuild" => Source::DotNet,
        _ => Source::Unknown,
    }
}

/// The errors in what a check printed (stdout and stderr together). `exe` and `args` say which program it was; for a script of a package manager, or a program not known here, the text says
/// whose it is. Warnings are not in the answer. Nothing is found in the output of a check that passed.
pub fn errors_from(exe: &str, args: &[String], output: &str) -> Vec<Diag> {
    let text = clean(output);
    let found = match source_of(exe, args) {
        Source::Rustc => rustc(&text),
        Source::Tsc => tsc(&text),
        Source::PyCompile => py_compile(&text),
        Source::Go => go(&text),
        Source::DotNet => dotnet(&text),
        Source::Unknown => {
            let mut found = tsc(&text);
            for read in [rustc, py_compile, dotnet, go] {
                if !found.is_empty() {
                    break;
                }
                found = read(&text);
            }
            found
        }
    };
    dedupe(found)
}

fn dedupe(found: Vec<Diag>) -> Vec<Diag> {
    let mut out: Vec<Diag> = Vec::with_capacity(found.len());
    for d in found {
        if !out.iter().any(|o| o.file == d.file && o.line == d.line && o.message == d.message) {
            out.push(d);
        }
    }
    out
}

fn rustc(text: &str) -> Vec<Diag> {
    parse::rustc::parse(text).failures.into_iter().map(Diag::from_failure).collect()
}

fn tsc(text: &str) -> Vec<Diag> {
    parse::tsc::parse(text, 1).failures.into_iter().map(Diag::from_failure).collect()
}

/// `python -m py_compile x.py` on a file that does not compile:
///
/// ```text
///   File "x.py", line 3
///     def f(:
///           ^
/// SyntaxError: invalid syntax
/// ```
///
/// and the older `Sorry: IndentationError: unexpected indent (x.py, line 3)`.
fn py_compile(text: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        // The frame of the error has no `, in name` after the line; the frames of a traceback that lead to it (`File "<frozen py_compile>", line 144, in compile`) have.
        if let Some(c) = re!(r#"^\s*File "(.+)", line (\d+)\s*$"#).captures(line) {
            // The error is named on one of the next lines.
            let named = lines[i + 1..lines.len().min(i + 8)].iter().find_map(|l| re!(r"^(\w*(?:Error|Exception))(?:: (.*))?$").captures(l).map(|e| (e[1].to_string(), e.get(2).map_or(String::new(), |m| m.as_str().to_string()))));
            if let Some((code, message)) = named {
                if code.ends_with("SyntaxError") || code == "IndentationError" || code == "TabError" || code.ends_with("Error") {
                    let message = if message.is_empty() { code.clone() } else { message };
                    out.push(Diag { file: Some(slash(&c[1])), line: parse::num(&c[2]), col: None, code, message: clip(&message, MAX_MESSAGE) });
                }
            }
        } else if let Some(c) = re!(r"^Sorry: (\w+Error): (.+?) \((.+), line (\d+)\)\s*$").captures(line) {
            out.push(Diag { file: Some(slash(&c[3])), line: parse::num(&c[4]), col: None, code: c[1].to_string(), message: clip(&c[2], MAX_MESSAGE) });
        }
    }
    out
}

/// `go build` and `go vet`: `./main.go:12:5: undefined: foo`, `vet: ./main.go:3:2: ...`, with `# package` lines between.
fn go(text: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(c) = re!(r"^(?:vet: )?(.+?\.go):(\d+):(\d+): (.+)$").captures(line) {
            out.push(Diag { file: Some(slash(&c[1])), line: parse::num(&c[2]), col: parse::num(&c[3]), code: String::new(), message: clip(&c[4], MAX_MESSAGE) });
        } else if let Some(c) = re!(r"^(?:vet: )?(.+?\.go):(\d+): (.+)$").captures(line) {
            // A test's own message (`    x_test.go:12: got 1`) has leading blanks; a compiler's has none.
            out.push(Diag { file: Some(slash(&c[1])), line: parse::num(&c[2]), col: None, code: String::new(), message: clip(&c[3], MAX_MESSAGE) });
        }
    }
    out
}

/// `dotnet build`: `C:\p\Program.cs(12,5): error CS1002: ; expected [C:\p\p.csproj]` (each error twice: in the log and in the summary).
fn dotnet(text: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(c) = re!(r"^(.+?)\((\d+),(\d+)(?:,\d+,\d+)?\): error ([A-Za-z]+\d+): (.+?)(?: \[[^\]]+\])?\s*$").captures(line) {
            out.push(Diag { file: Some(slash(&c[1])), line: parse::num(&c[2]), col: parse::num(&c[3]), code: c[4].to_string(), message: clip(&c[5], MAX_MESSAGE) });
        }
    }
    out
}

// ---- the block -------------------------------------------------------------------------------------------------------------------

/// The path as it is shown: forward slashes, and relative to `strip` (the folder of the project) when it is below it.
fn shown(file: &str, strip: Option<&str>) -> String {
    let f = slash(file);
    if let Some(root) = strip.map(slash) {
        let root = root.trim_end_matches('/').to_string();
        if !root.is_empty() && f.len() > root.len() + 1 && f[..root.len()].eq_ignore_ascii_case(&root) && f.as_bytes()[root.len()] == b'/' {
            return f[root.len() + 1..].to_string();
        }
    }
    f.strip_prefix("./").unwrap_or(&f).to_string()
}

/// The same file under another spelling (one relative, one absolute): the longer path ends with the shorter.
fn same_file(a: &str, b: &str) -> bool {
    let (a, b) = (slash(a).to_ascii_lowercase(), slash(b).to_ascii_lowercase());
    let (a, b) = (a.trim_start_matches("./").to_string(), b.trim_start_matches("./").to_string());
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

fn attribute(name: &str) -> String {
    name.chars().map(|c| if matches!(c, '"' | '<' | '>' | '\n' | '\r') { '_' } else { c }).collect()
}

/// A message that cannot close the block it is in or open another one.
fn safe(text: &str) -> String {
    text.replace("</", "<\\/").replace("<diagnostics", "<\\diagnostics")
}

/// The block for `diags`: one `<diagnostics file="...">` per file, the files in `changed_first` (the files this run changed, in the order it changed them) before the others, at most
/// [`MAX_PER_FILE`] errors for each and [`BUDGET_TOKENS`] in all, with a line that says how many were left out. `strip` is the folder the paths are shown relative to. `None` when there is
/// no error to show.
pub fn render(diags: &[Diag], changed_first: &[String], strip: Option<&str>) -> Option<String> {
    if diags.is_empty() {
        return None;
    }
    // The files in the order they are shown: those the run changed (in its order), then the others as they came.
    let mut order: Vec<(Option<String>, Vec<&Diag>)> = Vec::new();
    for d in diags {
        let key = d.file.as_ref().map(|f| shown(f, strip));
        match order.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(d),
            None => order.push((key, vec![d])),
        }
    }
    let rank = |file: &Option<String>| match file {
        Some(f) => changed_first.iter().position(|c| same_file(c, f)).unwrap_or(usize::MAX),
        None => usize::MAX,
    };
    order.sort_by_key(|(file, _)| rank(file));
    let total = diags.len();
    let mut out = String::new();
    let mut shown_errors = 0usize;
    // The errors of a file that the cap of [`MAX_PER_FILE`] cut, which the block says itself.
    let mut said_in_blocks = 0usize;
    for (file, list) in &order {
        let open = match file {
            Some(f) => format!("<diagnostics file=\"{}\">\n", attribute(f)),
            None => "<diagnostics>\n".to_string(),
        };
        let close = "</diagnostics>\n";
        let mut body = String::new();
        let mut kept = 0usize;
        let mut out_of_room = false;
        for d in list.iter().take(MAX_PER_FILE) {
            let line = format!("{}\n", safe(&d.text()));
            // The first error is always shown; every other one only if the whole block still fits.
            if shown_errors + kept > 0 && tokens_of(&format!("{out}{open}{body}{line}{close}")) > BUDGET_TOKENS {
                out_of_room = true;
                break;
            }
            body.push_str(&line);
            kept += 1;
        }
        if kept > 0 {
            if !out_of_room && list.len() > MAX_PER_FILE {
                body.push_str(&format!("... and {} more in this file\n", list.len() - MAX_PER_FILE));
                said_in_blocks += list.len() - MAX_PER_FILE;
            }
            out.push_str(&open);
            out.push_str(&body);
            out.push_str(close);
            shown_errors += kept;
        }
        if out_of_room {
            break;
        }
    }
    let left = total.saturating_sub(shown_errors + said_in_blocks);
    if left > 0 {
        out.push_str(&format!("({left} more errors not shown)\n"));
    }
    Some(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(exe: &str, args: &[&str], text: &str) -> Vec<Diag> {
        errors_from(exe, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), text)
    }

    #[test]
    fn cargo_short_lines_become_a_block() {
        let out = "    Checking aqua v0.1.0 (E:\\w)\nsrc/x.rs:12:5: error[E0308]: mismatched types\nsrc/x.rs:30:9: warning: unused variable: `y`\nsrc/lib.rs:3:1: error: expected item, found `}`\nerror: could not compile `aqua` (lib) due to 2 previous errors\n";
        let d = run("cargo", &["check", "--offline", "--message-format", "short"], out);
        assert_eq!(d.len(), 2, "{d:?}");
        let block = render(&d, &["E:/w/src/x.rs".into()], Some("E:/w")).unwrap();
        assert_eq!(block, "<diagnostics file=\"src/x.rs\">\nERROR [12:5] mismatched types (E0308)\n</diagnostics>\n<diagnostics file=\"src/lib.rs\">\nERROR [3:1] expected item, found `}`\n</diagnostics>");
    }

    #[test]
    fn tsc_lines_become_a_block() {
        let out = "src/a.ts(3,5): error TS2322: Type 'string' is not assignable to type 'number'.\nsrc/a.ts(9,1): error TS1005: ';' expected.\n\nFound 2 errors in the same file, starting at: src/a.ts:3\n";
        for (exe, args) in [("tsc", vec!["--noEmit"]), ("npm", vec!["run", "typecheck", "--if-present"])] {
            let d = run(exe, &args, out);
            assert_eq!(d.len(), 2, "{exe}: {d:?}");
        }
        let block = render(&run("npm", &["run", "typecheck"], out), &[], None).unwrap();
        assert!(block.starts_with("<diagnostics file=\"src/a.ts\">\nERROR [3:5] Type 'string' is not assignable to type 'number'. (TS2322)\nERROR [9:1] ';' expected. (TS1005)\n</diagnostics>"), "{block}");
    }

    #[test]
    fn go_vet_lines_become_a_block() {
        let out = "# example.com/app\n./main.go:12:5: undefined: foo\nvet: ./util/u.go:3:2: declared and not used: x\n    handler_test.go:20: expected 1, got 2\n";
        let d = run("go", &["build", "./..."], out);
        assert_eq!(d.len(), 3, "{d:?}");
        assert_eq!((d[0].file.as_deref(), d[0].line, d[0].col), (Some("./main.go"), Some(12), Some(5)));
        let block = render(&d, &[], None).unwrap();
        assert!(block.contains("<diagnostics file=\"main.go\">\nERROR [12:5] undefined: foo\n</diagnostics>"), "{block}");
        // A test run is not a build: its failures are the test parsers' business.
        assert!(run("go", &["test", "./..."], out).iter().all(|d| d.file.is_some()), "go test falls back to reading the text");
    }

    #[test]
    fn dotnet_lines_become_a_block() {
        let out = "  Determining projects to restore...\nC:\\p\\Program.cs(12,5): error CS1002: ; expected [C:\\p\\p.csproj]\nC:\\p\\Program.cs(14,9): warning CS0168: The variable 'e' is declared but never used [C:\\p\\p.csproj]\nBuild FAILED.\nC:\\p\\Program.cs(12,5): error CS1002: ; expected [C:\\p\\p.csproj]\n    1 Error(s)\n";
        let d = run("dotnet", &["build"], out);
        assert_eq!(d.len(), 1, "an error that is printed twice is one: {d:?}");
        let block = render(&d, &["C:/p/Program.cs".into()], Some("C:\\p")).unwrap();
        assert_eq!(block, "<diagnostics file=\"Program.cs\">\nERROR [12:5] ; expected (CS1002)\n</diagnostics>");
    }

    #[test]
    fn py_compile_traceback_becomes_a_block() {
        let out = "  File \"tool_b.py\", line 3\n    def f(:\n          ^\nSyntaxError: invalid syntax\n";
        let d = run("python", &["-m", "py_compile", "tool_b.py"], out);
        assert_eq!(d, vec![Diag { file: Some("tool_b.py".into()), line: Some(3), col: None, code: "SyntaxError".into(), message: "invalid syntax".into() }]);
        let block = render(&d, &[], None).unwrap();
        assert_eq!(block, "<diagnostics file=\"tool_b.py\">\nERROR [3] invalid syntax (SyntaxError)\n</diagnostics>");
        // The traceback Python prints for a file that does not compile: the frames that lead to the error are not the error.
        let traceback = "Traceback (most recent call last):\n  File \"<frozen py_compile>\", line 144, in compile\n  File \"calc.py\", line 3\n    return sum(values) +\n                        ^\nSyntaxError: invalid syntax\n";
        let d = run("python", &["-m", "py_compile", "calc.py"], traceback);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!((d[0].file.as_deref(), d[0].line, d[0].message.as_str()), (Some("calc.py"), Some(3), "invalid syntax"));
        let old = "Sorry: IndentationError: unexpected indent (x.py, line 7)\n";
        assert_eq!(run("python", &["-m", "py_compile", "x.py"], old)[0].line, Some(7));
        // A program that is not a compile of one file, with the same text, is read by what the text is.
        assert_eq!(run("some-wrapper", &["go"], out).len(), 1);
    }

    #[test]
    fn warnings_are_left_out_and_a_clean_run_has_nothing() {
        let only_warnings = "src/x.rs:30:9: warning: unused variable: `y`\nwarning: `aqua` (lib) generated 1 warning\n";
        assert!(run("cargo", &["check"], only_warnings).is_empty());
        assert!(render(&[], &[], None).is_none());
        assert!(run("cargo", &["check"], "    Finished dev [unoptimized] target(s) in 0.4s\n").is_empty());
        assert!(run("tsc", &["--noEmit"], "").is_empty());
    }

    #[test]
    fn at_most_twenty_per_file() {
        let lines: String = (1..=30).map(|n| format!("src/x.rs:{n}:1: error[E0425]: cannot find value `v{n}` in this scope\n")).collect();
        let d = run("cargo", &["check"], &lines);
        assert_eq!(d.len(), 30);
        let block = render(&d, &[], None).unwrap();
        assert_eq!(block.matches("ERROR [").count(), MAX_PER_FILE, "{block}");
        assert!(block.contains("... and 10 more in this file"), "{block}");
        assert!(!block.contains("more errors not shown"), "the ten are said once: {block}");
    }

    #[test]
    fn budget_is_kept() {
        let long = "x".repeat(250);
        let mut text = String::new();
        for f in 1..=12 {
            for n in 1..=5 {
                text.push_str(&format!("src/f{f}.rs:{n}:1: error[E0001]: {long}\n"));
            }
        }
        let d = run("cargo", &["check"], &text);
        assert_eq!(d.len(), parse::MAX_FAILURES, "the parsers keep the first fifty");
        let block = render(&d, &[], None).unwrap();
        assert!(tokens_of(&block) <= BUDGET_TOKENS + 12, "{} tokens", tokens_of(&block));
        assert!(block.contains("more errors not shown"), "{block}");
        assert_eq!(block.matches("<diagnostics").count(), block.matches("</diagnostics>").count(), "every block that was opened is closed: {block}");
        // The first error is shown whatever its size.
        let huge = Diag { file: Some("a.rs".into()), line: Some(1), col: None, code: String::new(), message: "y".repeat(5000) };
        assert!(render(&[huge], &[], None).unwrap().contains("ERROR [1] yyyy"));
    }

    #[test]
    fn the_files_the_run_changed_come_first_and_a_message_cannot_close_the_block() {
        let text = "src/other.rs:1:1: error: first\nsrc/mine.rs:2:2: error: second </diagnostics> <diagnostics file=\"x\">\n";
        let d = run("cargo", &["check"], text);
        let block = render(&d, &["/work/proj/src/mine.rs".into()], Some("/work/proj")).unwrap();
        assert!(block.starts_with("<diagnostics file=\"src/mine.rs\">"), "{block}");
        assert_eq!(block.matches("</diagnostics>").count(), 2, "{block}");
        assert_eq!(block.matches("<diagnostics").count(), 2, "{block}");
        // An error with no file is in a block of its own.
        let loose = render(&[Diag { file: None, line: None, col: None, code: String::new(), message: "linking failed".into() }], &[], None).unwrap();
        assert_eq!(loose, "<diagnostics>\nERROR linking failed\n</diagnostics>");
    }
}
