//! `python -m unittest`: the `FAIL:` and `ERROR:` blocks with their tracebacks, and the last lines (`Ran 4 tests in 0.002s`, `FAILED (failures=1, errors=1, skipped=1)`).

use super::{clip, push_failure, re, slash, Counts, Failure, Parsed, Tool, MAX_MESSAGE};

pub fn parse(text: &str) -> Parsed {
    let mut p = Parsed::new(Tool::Unittest);
    let lines: Vec<&str> = text.lines().collect();
    let mut ran: Option<u32> = None;
    let (mut failures, mut errors, mut skipped) = (0u32, 0u32, 0u32);
    let mut no_tests = false;
    for line in &lines {
        if let Some(c) = re!(r"^Ran (\d+) tests? in ").captures(line) {
            ran = super::num(&c[1]);
        } else if line.starts_with("NO TESTS RAN") {
            no_tests = true;
        } else if let Some(c) = re!(r"^(?:OK|FAILED)(?: \((.*)\))?$").captures(line) {
            if let Some(inner) = c.get(1) {
                for kv in inner.as_str().split(',') {
                    if let Some((k, v)) = kv.trim().split_once('=') {
                        let n = super::num(v).unwrap_or(0);
                        match k.trim() {
                            "failures" => failures = n,
                            "errors" => errors = n,
                            "skipped" => skipped = n,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    if let Some(total) = ran {
        let failed = failures + errors;
        p.counts = Some(Counts { passed: total.saturating_sub(failed + skipped), failed, skipped });
        p.ran_any = Some(total.saturating_sub(skipped) > 0 && !no_tests);
        p.recognised = true;
    }
    // The blocks: a line of `=`, `FAIL: name (module.Class.name)` or `ERROR: ...`, a line of `-`, then the traceback up to the next block.
    let mut i = 0;
    while i < lines.len() {
        if let Some(c) = re!(r"^(FAIL|ERROR): (\S+) \((.+)\)$").captures(lines[i]) {
            let name = c[2].to_string();
            let dotted = c[3].to_string();
            let id = if dotted.ends_with(&format!(".{name}")) { dotted } else { format!("{dotted}.{name}") };
            let mut j = i + 1;
            while j < lines.len() && !lines[j].starts_with("=====") && !re!(r"^Ran \d+ tests? in ").is_match(lines[j]) {
                j += 1;
            }
            let body = &lines[i + 1..j];
            // The deepest frame of the traceback is the last `File "...", line N` of it.
            let frame = body.iter().rev().find_map(|l| re!(r#"^\s*File "(.+)", line (\d+)"#).captures(l));
            // The message is the line after the traceback that starts in the first column (`AssertionError: ...`, `ValueError: ...`).
            let after = body.iter().rposition(|l| re!(r#"^\s*File ".+", line \d+"#).is_match(l)).map_or(0, |k| k + 1);
            let message = body[after..].iter().find(|l| !l.is_empty() && !l.starts_with(char::is_whitespace) && !l.starts_with("---")).map_or("", |l| l);
            push_failure(
                &mut p,
                Failure { id, file: frame.as_ref().map(|f| slash(&f[1])), line: frame.as_ref().and_then(|f| super::num(&f[2])), col: None, message: clip(message, MAX_MESSAGE) },
            );
            i = j;
        } else {
            i += 1;
        }
    }
    if !p.failures.is_empty() {
        p.recognised = true;
    }
    p
}
