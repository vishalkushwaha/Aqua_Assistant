//! `ruff check`: the concise form (`file:line:col: F401 message`), the default form (`F401 message`, then ` --> file:line:col`) and the JSON form (`--output-format json`).

use super::{clip, push_failure, re, slash, Failure, Parsed, Tool, MAX_MESSAGE};
use serde_json::Value;

pub fn parse(text: &str, exit: i32) -> Parsed {
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        if let Some(p) = parse_json(trimmed) {
            return p;
        }
    }
    let mut p = Parsed::new(Tool::Ruff);
    let lines: Vec<&str> = text.lines().collect();
    let mut total: Option<u32> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(c) = re!(r"^(.+?):(\d+):(\d+): ([A-Z]+\d+) (?:\[\*\] )?(.+)$").captures(line) {
            push_failure(&mut p, Failure { id: c[4].to_string(), file: Some(slash(&c[1])), line: super::num(&c[2]), col: super::num(&c[3]), message: clip(&c[5], MAX_MESSAGE) });
        } else if let Some(c) = re!(r"^([A-Z]+\d+) (?:\[\*\] )?(.+)$").captures(line) {
            // The default form: the place is on the next line.
            if let Some(at) = lines.get(i + 1).and_then(|l| re!(r"^\s*--> (.+?):(\d+):(\d+)\s*$").captures(l)) {
                push_failure(&mut p, Failure { id: c[1].to_string(), file: Some(slash(&at[1])), line: super::num(&at[2]), col: super::num(&at[3]), message: clip(&c[2], MAX_MESSAGE) });
            }
        } else if let Some(c) = re!(r"^Found (\d+) errors?\.").captures(line) {
            total = super::num(&c[1]);
        } else if line.starts_with("All checks passed") {
            total = Some(0);
        }
    }
    p.error_total = total.or_else(|| (!p.failures.is_empty()).then(|| p.failures.len() as u32));
    p.recognised = !p.failures.is_empty() || total.is_some() || (exit == 0 && text.trim().is_empty());
    p
}

fn parse_json(text: &str) -> Option<Parsed> {
    let end = text.rfind(']')?;
    let findings: Vec<Value> = serde_json::from_str(&text[..=end]).ok()?;
    let mut p = Parsed::new(Tool::Ruff);
    for f in &findings {
        push_failure(
            &mut p,
            Failure {
                id: f.get("code").and_then(Value::as_str).unwrap_or("ruff").to_string(),
                file: f.get("filename").and_then(Value::as_str).map(slash),
                line: f.pointer("/location/row").and_then(Value::as_u64).map(|n| n as u32),
                col: f.pointer("/location/column").and_then(Value::as_u64).map(|n| n as u32),
                message: clip(f.get("message").and_then(Value::as_str).unwrap_or(""), MAX_MESSAGE),
            },
        );
    }
    p.error_total = Some(findings.len() as u32);
    p.recognised = true;
    Some(p)
}
