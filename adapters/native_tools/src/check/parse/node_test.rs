//! `node --test`: the `spec` reporter (`✔ name`, `✖ name`, the `ℹ tests 4` lines, the `failing tests:` block) and the TAP form (`not ok 2 - name`, `# fail 2`).

use super::{clip, push_failure, re, slash, Counts, Failure, Parsed, Tool, MAX_MESSAGE};

pub fn parse(text: &str) -> Parsed {
    let mut p = Parsed::new(Tool::NodeTest);
    let lines: Vec<&str> = text.lines().collect();
    let (mut tests, mut pass, mut fail, mut cancelled, mut skipped, mut todo) = (None, 0u32, 0u32, 0u32, 0u32, 0u32);
    for line in &lines {
        let spec = re!(r"^ℹ (tests|pass|fail|cancelled|skipped|todo) (\d+)\s*$").captures(line);
        let tap = re!(r"^# (tests|pass|fail|cancelled|skipped|todo) (\d+)\s*$").captures(line);
        if let Some(c) = spec.or(tap) {
            let n = super::num(&c[2]).unwrap_or(0);
            match &c[1] {
                "tests" => tests = Some(n),
                "pass" => pass = n,
                "fail" => fail = n,
                "cancelled" => cancelled = n,
                "skipped" => skipped = n,
                _ => todo = n,
            }
        }
    }
    if let Some(total) = tests {
        p.recognised = true;
        p.counts = Some(Counts { passed: pass, failed: fail + cancelled, skipped: skipped + todo });
        p.ran_any = Some(total > 0 && pass + fail + cancelled > 0);
    }
    // The block `✖ failing tests:`, each entry `test at file:line:col`, `✖ name (1.2ms)`, the message and the stack.
    let start = lines.iter().position(|l| l.contains("failing tests:"));
    if let Some(s) = start {
        let mut i = s + 1;
        while i < lines.len() {
            let Some(at) = re!(r"^test at (.+?):(\d+):(\d+)\s*$").captures(lines[i]) else {
                i += 1;
                continue;
            };
            let (tfile, tline, tcol) = (slash(&at[1]), super::num(&at[2]), super::num(&at[3]));
            let mut j = i + 1;
            while j < lines.len() && !re!(r"^test at .+:\d+:\d+\s*$").is_match(lines[j]) {
                j += 1;
            }
            let body = &lines[i + 1..j];
            let name = body.first().and_then(|l| re!(r"^✖ (.+?)(?: \([\d.]+m?s\))?\s*$").captures(l)).map_or("test".to_string(), |c| c[1].to_string());
            let message_lines: Vec<&str> = body.iter().skip(1).map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with("at ")).take(3).collect();
            let message = match message_lines.as_slice() {
                [first, second, ..] if first.ends_with(':') => format!("{} {second}", first.trim_end_matches(':')),
                [first, ..] => first.to_string(),
                [] => String::new(),
            };
            // The first stack frame that is in the project and not in Node's own files.
            let frame = body.iter().find_map(|l| {
                let c = re!(r"^\s+at .*?\(?([^()\s]+?):(\d+):(\d+)\)?\s*$").captures(l)?;
                (!c[1].starts_with("node:") && !c[1].contains("node_modules")).then(|| (slash(&c[1]), super::num(&c[2]), super::num(&c[3])))
            });
            let (file, line, col) = match frame {
                Some((f, l, c)) => (Some(f), l, c),
                None => (Some(tfile), tline, tcol),
            };
            push_failure(&mut p, Failure { id: name, file, line, col, message: clip(&message, MAX_MESSAGE) });
            i = j;
        }
    }
    // No such block (a TAP report, or a reporter with no details): the failed lines name them.
    if p.failures.is_empty() {
        for line in &lines {
            let name = re!(r"^✖ (.+?)(?: \([\d.]+m?s\))?\s*$").captures(line).map(|c| c[1].to_string()).or_else(|| re!(r"^not ok \d+ - (.+?)\s*$").captures(line).map(|c| c[1].to_string()));
            if let Some(name) = name {
                if !name.starts_with("failing tests") {
                    push_failure(&mut p, Failure { id: name, file: None, line: None, col: None, message: String::new() });
                }
            }
        }
        if !p.failures.is_empty() {
            p.recognised = true;
        }
    }
    p
}
