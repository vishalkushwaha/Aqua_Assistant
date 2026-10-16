//! `go test`, `go build`, `go vet`: `--- FAIL: TestName (0.00s)` with the indented `file.go:14: message` under it, `panic:` lines, build errors (`file.go:12:9: message`) and the package
//! lines (`ok  	pkg	0.004s`, `FAIL	pkg	0.004s`, `?   	pkg	[no test files]`).

use super::{clip, push_failure, re, slash, Counts, Failure, Parsed, Tool, MAX_MESSAGE};

pub fn parse(text: &str) -> Parsed {
    let mut p = Parsed::new(Tool::GoTest);
    let lines: Vec<&str> = text.lines().collect();
    let (mut ok_pkgs, mut failed_pkgs, mut empty_pkgs) = (0u32, 0u32, 0u32);
    for line in &lines {
        if re!(r"^ok\s+\S+\s").is_match(line) {
            ok_pkgs += 1;
        } else if re!(r"^FAIL\s+\S+(\s|$)").is_match(line) {
            failed_pkgs += 1;
        } else if re!(r"^\?\s+\S+\s+\[no test files\]").is_match(line) {
            empty_pkgs += 1;
        }
    }
    let mut failed_tests = 0u32;
    let mut in_build_errors = false;
    for (i, line) in lines.iter().enumerate() {
        if let Some(c) = re!(r"^--- FAIL: (\S+) \(").captures(line) {
            failed_tests += 1;
            // The first indented line under it that names a place: `    parse_test.go:14: got 3, want 4`.
            let place = lines[i + 1..].iter().take_while(|l| l.starts_with(' ') || l.starts_with('\t')).find_map(|l| re!(r"^\s+(\S+?\.go):(\d+):(?:\d+:)? ?(.*)$").captures(l));
            let (file, ln, message) = match place {
                Some(m) => (Some(slash(&m[1])), super::num(&m[2]), m[3].to_string()),
                None => (None, None, String::new()),
            };
            push_failure(&mut p, Failure { id: c[1].to_string(), file, line: ln, col: None, message: clip(&message, MAX_MESSAGE) });
        } else if let Some(c) = re!(r"^panic: (.+)$").captures(line) {
            // The frame of the test or the code that panicked is the first `file.go:line` of a project file in the trace.
            let frame = lines[i + 1..].iter().find_map(|l| re!(r"^\s+(\S+?\.go):(\d+)").captures(l).filter(|m| !m[1].contains("/usr/local/go/") && !m[1].contains("/go/src/")));
            push_failure(&mut p, Failure { id: "panic".into(), file: frame.as_ref().map(|m| slash(&m[1])), line: frame.as_ref().and_then(|m| super::num(&m[2])), col: None, message: clip(&c[1], MAX_MESSAGE) });
        } else if line.starts_with("# ") && !line.starts_with("# [") {
            // `# example.com/proj/parse [example.com/proj/parse.test]`: the lines of the compiler follow.
            in_build_errors = true;
        } else if in_build_errors {
            match re!(r"^(.+?\.go):(\d+):(\d+): (.+)$").captures(line) {
                Some(c) => push_failure(&mut p, Failure { id: "build".into(), file: Some(slash(&c[1])), line: super::num(&c[2]), col: super::num(&c[3]), message: clip(&c[4], MAX_MESSAGE) }),
                None => in_build_errors = false,
            }
        }
    }
    let any = ok_pkgs + failed_pkgs + empty_pkgs > 0;
    if any || !p.failures.is_empty() {
        p.recognised = true;
        // Without `-v` go prints no count of the tests that passed: only what failed is known, and that a package ran tests.
        p.counts = Some(Counts { passed: 0, failed: failed_tests, skipped: 0 });
        p.ran_any = Some(ok_pkgs + failed_pkgs > 0 || failed_tests > 0);
        p.error_total = Some(failed_tests.max(p.failures.len() as u32));
    }
    p
}
