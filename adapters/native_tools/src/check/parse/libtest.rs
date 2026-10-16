//! `cargo test` (the test harness of Rust, libtest): `test name ... FAILED`, the `---- name stdout ----` sections with the panic and its place, and one `test result:` line per target.

use super::{cap_num, clip, push_failure, re, rustc, slash, Counts, Failure, Parsed, Tool, MAX_MESSAGE};

/// One `---- name stdout ----` section: the place of the panic and what it said.
struct Section {
    name: String,
    file: Option<String>,
    line: Option<u32>,
    col: Option<u32>,
    message: String,
}

pub fn parse(text: &str) -> Parsed {
    let mut p = Parsed::new(Tool::Libtest);
    let mut counts = Counts::default();
    let mut results = 0u32;
    let mut ran_lines = 0u32;
    let mut failed_names: Vec<String> = Vec::new();
    let mut listed: Vec<String> = Vec::new();
    let mut in_list = false;
    for line in text.lines() {
        if let Some(c) = re!(r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored").captures(line) {
            counts.passed += cap_num(&c, 1);
            counts.failed += cap_num(&c, 2);
            counts.skipped += cap_num(&c, 3);
            results += 1;
            in_list = false;
        } else if re!(r"^running \d+ tests?$").is_match(line) {
            ran_lines += 1;
        } else if let Some(c) = re!(r"^test (.+?) \.\.\. FAILED").captures(line) {
            failed_names.push(c[1].to_string());
        } else if line == "failures:" {
            in_list = true;
        } else if in_list {
            match line.strip_prefix("    ") {
                Some(name) if !name.trim().is_empty() && !name.contains(' ') => listed.push(name.trim().to_string()),
                _ => {
                    if !line.trim().is_empty() {
                        in_list = false;
                    }
                }
            }
        }
    }
    if results == 0 && ran_lines == 0 {
        // Nothing ran: the program did not get as far as the tests. A compile error is the usual reason, and it is read as the compiler says it.
        let r = rustc::parse(text);
        if !r.failures.is_empty() || r.error_total.is_some() {
            p.recognised = true;
            p.failures = r.failures;
            p.error_total = r.error_total;
        }
        return p;
    }
    p.recognised = true;
    p.counts = Some(counts);
    p.ran_any = Some(counts.passed + counts.failed > 0);
    let names = if failed_names.is_empty() { listed } else { failed_names };
    let mut sections = sections_of(text);
    let mut seen: Vec<String> = Vec::new();
    for name in names {
        let again = seen.iter().filter(|n| **n == name).count();
        seen.push(name.clone());
        let at = sections.iter().position(|s| s.name == name);
        let section = at.map(|i| sections.remove(i));
        let id = if again == 0 { name.clone() } else { format!("{name} ({})", again + 1) };
        let (file, line, col, message) = match section {
            Some(s) => (s.file, s.line, s.col, s.message),
            None => (None, None, None, String::new()),
        };
        push_failure(&mut p, Failure { id, file, line, col, message: clip(&message, MAX_MESSAGE) });
    }
    p
}

/// The `---- name stdout ----` sections, in order.
fn sections_of(text: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    let flush = |cur: Option<(String, Vec<&str>)>, out: &mut Vec<Section>| {
        if let Some((name, lines)) = cur {
            out.push(read_section(name, &lines));
        }
    };
    for line in text.lines() {
        if let Some(c) = re!(r"^---- (.+) stdout ----$").captures(line) {
            let prev = current.take();
            flush(prev, &mut out);
            current = Some((c[1].to_string(), Vec::new()));
        } else if line == "failures:" || line.starts_with("test result:") {
            let prev = current.take();
            flush(prev, &mut out);
        } else if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    flush(current.take(), &mut out);
    out
}

/// The place and the message of a panic in the lines of one section: both the form of today's Rust (`thread 'x' (123) panicked at file:1:2:` and the message on the next line) and the older one
/// (`thread 'x' panicked at 'message', file:1:2`).
fn read_section(name: String, lines: &[&str]) -> Section {
    let mut s = Section { name, file: None, line: None, col: None, message: String::new() };
    for (i, l) in lines.iter().enumerate() {
        // The older form first: its last words are a place too.
        if let Some(c) = re!(r"panicked at '(.*)', (.+?):(\d+):(\d+)").captures(l) {
            s.message = c[1].to_string();
            s.file = Some(slash(&c[2]));
            s.line = super::num(&c[3]);
            s.col = super::num(&c[4]);
            return s;
        }
        if let Some(c) = re!(r"panicked at (.+?):(\d+):(\d+):?$").captures(l) {
            s.file = Some(slash(&c[1]));
            s.line = super::num(&c[2]);
            s.col = super::num(&c[3]);
            s.message = message_after(&lines[i + 1..]);
            return s;
        }
    }
    // No panic line (a test that should have panicked and did not, a process that ended): what the section says first.
    s.message = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("").to_string();
    s
}

/// The lines of the message of a panic: up to the note about the backtrace or the next blank line. An assertion's two sides are put on its first line.
fn message_after(lines: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for l in lines {
        let t = l.trim();
        if t.starts_with("note:") || (t.is_empty() && !parts.is_empty()) {
            break;
        }
        if !t.is_empty() {
            parts.push(t.to_string());
        }
        if parts.len() >= 4 {
            break;
        }
    }
    match parts.first().map(String::as_str) {
        Some(first) if first.starts_with("assertion `left ") && parts.len() >= 3 => {
            let left = parts.iter().find_map(|l| l.strip_prefix("left:")).unwrap_or("").trim();
            let right = parts.iter().find_map(|l| l.strip_prefix("right:")).unwrap_or("").trim();
            // `assertion `left == right` failed` or `assertion `left == right` failed: the message of the test`.
            let (what, note) = first.strip_prefix("assertion `").and_then(|r| r.split_once('`')).map_or((first, ""), |(w, rest)| (w, rest.trim_start_matches(" failed").trim_start_matches(':').trim()));
            let note = if note.is_empty() { String::new() } else { format!(" ({note})") };
            format!("{what} failed{note}: left {left}, right {right}")
        }
        _ => parts.join(" "),
    }
}
