//! `tsc --noEmit` (`--pretty false`): `file(line,col): error TS2322: message`, and the pretty form `file:line:col - error TS2322: message`. A clean run prints nothing.

use super::{clip, push_failure, re, slash, Failure, Parsed, Tool, MAX_MESSAGE};

pub fn parse(text: &str, exit: i32) -> Parsed {
    let mut p = Parsed::new(Tool::Tsc);
    let mut total: Option<u32> = None;
    for line in text.lines() {
        let plain = re!(r"^(.+?)\((\d+),(\d+)\): error (TS\d+): (.+)$").captures(line);
        let pretty = re!(r"^(.+?):(\d+):(\d+) - error (TS\d+): (.+)$").captures(line);
        if let Some(c) = plain.or(pretty) {
            push_failure(&mut p, Failure { id: c[4].to_string(), file: Some(slash(&c[1])), line: super::num(&c[2]), col: super::num(&c[3]), message: clip(&c[5], MAX_MESSAGE) });
        } else if let Some(c) = re!(r"^Found (\d+) errors?").captures(line) {
            total = super::num(&c[1]);
        } else if let Some(c) = re!(r"error (TS\d+): (.+)$").captures(line) {
            // An error with no place (a bad option, a file that is missing): `error TS5058: The specified path does not exist: 'x'.`
            push_failure(&mut p, Failure { id: c[1].to_string(), file: None, line: None, col: None, message: clip(&c[2], MAX_MESSAGE) });
        }
    }
    p.error_total = total.or_else(|| (!p.failures.is_empty()).then(|| p.failures.len() as u32));
    // tsc says nothing when all is well: an empty output with exit 0 is a clean run of it.
    p.recognised = !p.failures.is_empty() || total.is_some() || (exit == 0 && text.trim().is_empty());
    p
}
