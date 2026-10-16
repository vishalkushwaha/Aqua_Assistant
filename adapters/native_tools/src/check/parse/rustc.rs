//! The Rust compiler through `cargo build|check|clippy`: the short form (`path:line:col: error[E0308]: message`) and the human form (`error[E0308]: message` then ` --> path:line:col`).

use super::{cap_num, clip, push_failure, re, slash, Failure, Parsed, Tool, MAX_MESSAGE};

/// What the lines of the human form say after `error`, that are no error of the code: the summary lines of cargo.
fn is_summary(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.starts_with("could not compile")
        || m.starts_with("aborting due to")
        || m.starts_with("test failed")
        || m.starts_with("process didn't exit")
        || m.starts_with("build failed")
        || m.starts_with("some errors have detailed")
        || m.starts_with("for more information")
}

pub fn parse(text: &str) -> Parsed {
    let mut p = Parsed::new(Tool::Rustc);
    let lines: Vec<&str> = text.lines().collect();
    let mut total: Option<u32> = None;
    let mut seen_error = false;
    for (i, line) in lines.iter().enumerate() {
        if let Some(c) = re!(r"due to (\d+) previous errors?").captures(line) {
            total = Some(cap_num(&c, 1));
            continue;
        }
        if let Some(c) = re!(r"^(.+?):(\d+):(\d+): (error|warning)(?:\[(\w+)\])?: (.+)$").captures(line) {
            if &c[4] == "error" {
                seen_error = true;
                let message = c[6].to_string();
                push_failure(
                    &mut p,
                    Failure { id: c.get(5).map_or("error".to_string(), |m| m.as_str().to_string()), file: Some(slash(&c[1])), line: super::num(&c[2]), col: super::num(&c[3]), message: clip(&message, MAX_MESSAGE) },
                );
            }
            continue;
        }
        if let Some(c) = re!(r"^error(?:\[(\w+)\])?: (.+)$").captures(line) {
            let message = c[2].to_string();
            if is_summary(&message) {
                continue;
            }
            seen_error = true;
            // The place is on one of the next lines: ` --> path:line:col`.
            let place = lines[i + 1..lines.len().min(i + 5)].iter().find_map(|l| re!(r"^\s*--> (.+?):(\d+):(\d+)\s*$").captures(l).map(|c| (slash(&c[1]), super::num(&c[2]), super::num(&c[3]))));
            let (file, ln, col) = match place {
                Some((f, l, c)) => (Some(f), l, c),
                None => (None, None, None),
            };
            push_failure(&mut p, Failure { id: c.get(1).map_or("error".to_string(), |m| m.as_str().to_string()), file, line: ln, col, message: clip(&message, MAX_MESSAGE) });
        }
    }
    if seen_error || total.is_some() {
        p.recognised = true;
        p.error_total = total.or(Some(p.failures.len() as u32));
    } else if re!(r"(?m)^\s*(Compiling|Checking|Finished) ").is_match(text) {
        // A build that finished: nothing failed.
        p.recognised = true;
    }
    p
}
