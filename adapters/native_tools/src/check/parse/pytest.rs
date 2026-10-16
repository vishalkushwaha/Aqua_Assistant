//! `python -m pytest` (best with `-q --tb=short`, and readable without): the last line with the counts, the `short test summary info` with one line per failure, and the sections that hold the place.

use super::{clip, push_failure, re, slash, Counts, Failure, Parsed, Tool, MAX_MESSAGE};

pub fn parse(text: &str) -> Parsed {
    let mut p = Parsed::new(Tool::Pytest);
    let lines: Vec<&str> = text.lines().collect();
    // The line with the counts: `=== 3 failed, 1 passed, 1 skipped in 0.20s ===`, `3 passed in 0.02s`, `no tests ran in 0.01s`.
    let mut counts = None;
    for line in lines.iter().rev() {
        let t = line.trim().trim_matches('=').trim();
        if re!(r"^no tests ran in [\d.]+s").is_match(t) {
            counts = Some(Counts::default());
            p.ran_any = Some(false);
            break;
        }
        if re!(r" in [\d.]+s( \(.*\))?$").is_match(t) && re!(r"\d+ (passed|failed|skipped|errors?|xfailed|xpassed|deselected|warnings?)").is_match(t) {
            let mut c = Counts::default();
            for m in re!(r"(\d+) (passed|failed|skipped|errors?|xfailed|xpassed|deselected|warnings?)").captures_iter(t) {
                let n = super::num(&m[1]).unwrap_or(0);
                match &m[2] {
                    "passed" | "xpassed" => c.passed += n,
                    "failed" | "error" | "errors" => c.failed += n,
                    "skipped" | "xfailed" => c.skipped += n,
                    _ => {}
                }
            }
            p.ran_any = Some(c.passed + c.failed > 0);
            counts = Some(c);
            break;
        }
    }
    p.counts = counts;
    // One section per failing test: its header, then its lines, up to the next header or the summary.
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    for line in &lines {
        if let Some(c) = re!(r"^_{3,} (.+?) _{3,}$").captures(line) {
            sections.push((c[1].to_string(), Vec::new()));
        } else if line.starts_with("====") {
            // A banner ends the section before it (`short test summary info`, `warnings summary`, the last line).
            if let Some((_, l)) = sections.last_mut() {
                l.push("\u{0}end");
            }
        } else if let Some((_, l)) = sections.last_mut() {
            if !l.last().is_some_and(|x| *x == "\u{0}end") {
                l.push(line);
            }
        }
    }
    // The summary lines: `FAILED path::Class::name - message`, `ERROR path::name - message`.
    for line in &lines {
        if let Some(c) = re!(r"^(FAILED|ERROR) (\S+?)(?: - (.*))?$").captures(line) {
            let id = c[2].to_string();
            let summary_message = c.get(3).map_or(String::new(), |m| m.as_str().to_string());
            let key = id.split("::").skip(1).collect::<Vec<_>>().join(".");
            let last = id.rsplit("::").next().unwrap_or(&id).to_string();
            let section = sections.iter().find(|(name, _)| (!key.is_empty() && *name == key) || *name == last || (name.starts_with("ERROR collecting") && name.ends_with(id.split("::").next().unwrap_or(""))));
            let (mut file, mut ln, mut message) = (id.split("::").next().map(slash), None, summary_message);
            if let Some((_, body)) = section {
                // The deepest place named in the section, and its first `E` line.
                if let Some(m) = body.iter().rev().find_map(|l| re!(r"^(\S+?\.py):(\d+): ").captures(l)) {
                    file = Some(slash(&m[1]));
                    ln = super::num(&m[2]);
                }
                if let Some(e) = body.iter().find_map(|l| re!(r"^E\s+(\S.*)$").captures(l)) {
                    message = e[1].to_string();
                }
            }
            push_failure(&mut p, Failure { id, file, line: ln, col: None, message: clip(&message, MAX_MESSAGE) });
        }
    }
    // A run with failures and no summary lines (pytest with `-rN`): the sections name them.
    if p.failures.is_empty() {
        for (name, body) in &sections {
            let place = body.iter().rev().find_map(|l| re!(r"^(\S+?\.py):(\d+): ").captures(l));
            let message = body.iter().find_map(|l| re!(r"^E\s+(\S.*)$").captures(l)).map_or(String::new(), |e| e[1].to_string());
            push_failure(
                &mut p,
                Failure { id: name.clone(), file: place.as_ref().map(|m| slash(&m[1])), line: place.as_ref().and_then(|m| super::num(&m[2])), col: None, message: clip(&message, MAX_MESSAGE) },
            );
        }
    }
    p.recognised = p.counts.is_some() || !p.failures.is_empty();
    p
}
