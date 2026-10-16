//! `eslint`: the JSON form (`-f json`, an array of files with their messages) and the default `stylish` form (a path, then `  1:7  error  message  rule`, then `✖ 4 problems (3 errors, 1 warning)`).

use super::{clip, push_failure, re, slash, Failure, Parsed, Tool, MAX_MESSAGE};
use serde_json::Value;

pub fn parse(text: &str, exit: i32) -> Parsed {
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        if let Some(p) = parse_json(trimmed) {
            return p;
        }
    }
    parse_stylish(text, exit)
}

fn parse_json(text: &str) -> Option<Parsed> {
    // What eslint printed may have a line of something else before the array; the array is read from its first bracket to its last.
    let end = text.rfind(']')?;
    let files: Vec<Value> = serde_json::from_str(&text[..=end]).ok()?;
    let mut p = Parsed::new(Tool::Eslint);
    let mut errors = 0u32;
    for f in &files {
        let path = f.get("filePath").and_then(Value::as_str).unwrap_or("");
        for m in f.get("messages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            if m.get("severity").and_then(Value::as_u64) != Some(2) {
                continue;
            }
            errors += 1;
            let id = m.get("ruleId").and_then(Value::as_str).unwrap_or("parse error").to_string();
            push_failure(
                &mut p,
                Failure {
                    id,
                    file: Some(slash(path)),
                    line: m.get("line").and_then(Value::as_u64).map(|n| n as u32),
                    col: m.get("column").and_then(Value::as_u64).map(|n| n as u32),
                    message: clip(m.get("message").and_then(Value::as_str).unwrap_or(""), MAX_MESSAGE),
                },
            );
        }
    }
    p.recognised = true;
    p.error_total = Some(errors);
    Some(p)
}

fn parse_stylish(text: &str, exit: i32) -> Parsed {
    let mut p = Parsed::new(Tool::Eslint);
    let mut file: Option<String> = None;
    let mut total: Option<u32> = None;
    for line in text.lines() {
        if let Some(c) = re!(r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.*?)(?:\s{2,}(\S+))?\s*$").captures(line) {
            if &c[3] == "error" {
                let id = c.get(5).map_or("parse error".to_string(), |m| m.as_str().to_string());
                push_failure(&mut p, Failure { id, file: file.clone(), line: super::num(&c[1]), col: super::num(&c[2]), message: clip(&c[4], MAX_MESSAGE) });
            }
        } else if let Some(c) = re!(r"^✖ \d+ problems? \((\d+) errors?, \d+ warnings?\)").captures(line) {
            total = super::num(&c[1]);
        } else if !line.trim().is_empty() && !line.starts_with(' ') && !line.starts_with('✖') {
            file = Some(slash(line.trim()));
        }
    }
    p.error_total = total.or_else(|| (!p.failures.is_empty()).then(|| p.failures.len() as u32));
    // eslint prints nothing when all is well.
    p.recognised = !p.failures.is_empty() || total.is_some() || (exit == 0 && text.trim().is_empty());
    p
}
