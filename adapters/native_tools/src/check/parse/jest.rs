//! jest and vitest. jest: `● suite › test` sections, the `at ... (file:line:col)` frame, `Tests: 1 failed, 5 passed, 6 total`. vitest: ` FAIL  file > suite > test`, ` ❯ file:line:col`,
//! `      Tests  1 failed | 3 passed (4)`.

use super::{clip, push_failure, re, slash, Counts, Failure, Parsed, Tool, MAX_MESSAGE};

pub fn parse(text: &str) -> Parsed {
    let mut p = Parsed::new(Tool::Jest);
    let lines: Vec<&str> = text.lines().collect();
    // The counts: `Tests:       2 failed, 1 skipped, 5 passed, 8 total` (jest), `      Tests  1 failed | 3 passed (4)` (vitest).
    for line in &lines {
        let jest = re!(r"^\s*Tests:\s+(.*)$").captures(line);
        let vitest = re!(r"^\s+Tests\s+(\d.*)$").captures(line);
        let Some(c) = jest.or(vitest) else { continue };
        let mut counts = Counts::default();
        for m in re!(r"(\d+) (failed|passed|skipped|todo)").captures_iter(&c[1]) {
            let n = super::num(&m[1]).unwrap_or(0);
            match &m[2] {
                "failed" => counts.failed += n,
                "passed" => counts.passed += n,
                _ => counts.skipped += n,
            }
        }
        p.ran_any = Some(counts.passed + counts.failed > 0);
        p.counts = Some(counts);
        p.recognised = true;
    }
    if text.contains("No tests found") || text.contains("No test files found") {
        p.recognised = true;
        p.ran_any = Some(false);
        p.counts.get_or_insert_with(Counts::default);
    }
    // jest: each `  ● name` header begins a section.
    let heads: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| re!(r"^  ● .+$").is_match(l)).map(|(i, _)| i).collect();
    for (n, &h) in heads.iter().enumerate() {
        let end = heads.get(n + 1).copied().unwrap_or(lines.len());
        let body: Vec<&str> = lines[h + 1..end].iter().take_while(|l| !re!(r"^(Test Suites:|Tests:|Snapshots:)").is_match(l)).copied().collect();
        let name = lines[h].trim().trim_start_matches('●').trim().to_string();
        if name.starts_with("Console") {
            continue;
        }
        let first = body.iter().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("");
        let expected = body.iter().find_map(|l| l.trim().strip_prefix("Expected:")).map(str::trim);
        let received = body.iter().find_map(|l| l.trim().strip_prefix("Received:")).map(str::trim);
        let message = match (expected, received) {
            (Some(e), Some(r)) => format!("{first}: expected {e}, received {r}"),
            _ => first.to_string(),
        };
        let frame = body.iter().find_map(|l| {
            let c = re!(r"^\s+at (?:.*?\()?([^()\s]+?):(\d+):(\d+)\)?\s*$").captures(l)?;
            (!c[1].contains("node_modules") && !c[1].starts_with("node:")).then(|| (slash(&c[1]), super::num(&c[2]), super::num(&c[3])))
        });
        let (file, line, col) = match frame {
            Some((f, l, c)) => (Some(f), l, c),
            None => (None, None, None),
        };
        push_failure(&mut p, Failure { id: name, file, line, col, message: clip(&message, MAX_MESSAGE) });
    }
    // vitest: ` FAIL  file > suite > test`, the assertion on the next line, ` ❯ file:line:col` after it.
    for (i, line) in lines.iter().enumerate() {
        let Some(c) = re!(r"^\s*FAIL\s+(\S+?)(?: > (.+))?\s*$").captures(line) else { continue };
        let Some(test) = c.get(2) else { continue };
        let tail = &lines[i + 1..lines.len().min(i + 14)];
        let message = tail.iter().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("");
        let place = tail.iter().find_map(|l| re!(r"^\s*❯ (\S+?):(\d+):(\d+)\s*$").captures(l));
        push_failure(
            &mut p,
            Failure {
                id: format!("{} > {}", slash(&c[1]), test.as_str()),
                file: Some(place.as_ref().map_or_else(|| slash(&c[1]), |m| slash(&m[1]))),
                line: place.as_ref().and_then(|m| super::num(&m[2])),
                col: place.as_ref().and_then(|m| super::num(&m[3])),
                message: clip(message, MAX_MESSAGE),
            },
        );
    }
    // A file that did not run at all has no section of its own: `FAIL src/x.test.js` and `● Test suite failed to run` say it (the second is a section already).
    if p.failures.is_empty() {
        for line in &lines {
            if let Some(c) = re!(r"^FAIL (\S+)\s*$").captures(line) {
                push_failure(&mut p, Failure { id: slash(&c[1]), file: Some(slash(&c[1])), line: None, col: None, message: "the test file failed".into() });
            }
        }
        if !p.failures.is_empty() {
            p.recognised = true;
        }
    }
    p
}
