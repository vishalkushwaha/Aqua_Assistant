//! The report of a research (card R2-10): the answer, the numbered sources, the contradictions found, the time it took and the tier used, as Markdown. Every sentence of the answer and
//! of the contradictions is a sentence of a source, quoted with its number; the rest are the report's own labels. The file names that a report may have are made here too.

use super::plan::Tier;
use super::{host_of, one_line, Answer, Conflict, Outcome, Quote};
use chrono::NaiveDateTime;
use std::time::Duration;

/// The longest question the heading shows.
const MAX_QUESTION_CHARS: usize = 200;
/// The longest name a person may give a report, and the longest part of a name that comes from the question.
const MAX_NAME_CHARS: usize = 80;
const MAX_SLUG_CHARS: usize = 40;

fn seconds_text(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        "under a second".to_string()
    } else {
        format!("{secs:.1} seconds")
    }
}

fn dated(date: &Option<String>) -> String {
    match date {
        Some(d) => format!("dated {}", one_line(d, 40)),
        None => "no date".to_string(),
    }
}

fn conflict_line(c: &Conflict) -> String {
    let what = match c.kind {
        super::verify::Kind::Number => "Numbers",
        super::verify::Kind::Date => "Dates",
    };
    let side = |q: &Quote| format!("[{}] \"{}\" ({})", q.id, q.text, dated(&q.date));
    let verdict = match c.newer {
        Some(id) => format!("The newer dated source, [{id}], is preferred."),
        None => "Neither is dated later than the other (or a date is missing), so neither is preferred.".to_string(),
    };
    format!("- {what}: {} and {}. {verdict}", side(&c.first), side(&c.second))
}

/// The report as Markdown.
pub fn render(o: &Outcome) -> String {
    let mut s = format!("# Research: {}\n\n", one_line(&o.question, MAX_QUESTION_CHARS));
    s.push_str(&format!("- Tier: {} ({}): {}\n", o.tier.label(), o.tier.describe(), o.why));
    s.push_str(&format!("- Date: {}\n", o.today.format("%-d %B %Y")));
    s.push_str(&format!("- Time: {}\n", seconds_text(o.elapsed)));
    if matches!(o.tier, Tier::T2 | Tier::T3) {
        if o.queries.is_empty() {
            s.push_str("- Searched: no search answered\n");
        } else {
            let asked = o.queries.iter().map(|q| format!("\"{}\"", one_line(q, 100))).collect::<Vec<_>>().join(", ");
            s.push_str(&format!("- Searched: {} {} ({asked})\n", o.queries.len(), if o.queries.len() == 1 { "query" } else { "queries" }));
        }
        let pages = if o.pages_read.is_empty() { "none".to_string() } else { o.pages_read.iter().map(|id| format!("[{id}]")).collect::<Vec<_>>().join(", ") };
        s.push_str(&format!("- Pages read: {pages}\n"));
    }
    s.push_str("\n## Answer\n\n");
    s.push_str(&o.answer_text());
    s.push('\n');
    if matches!(o.answer, Answer::Found(_)) {
        if !o.conflicts.is_empty() {
            s.push_str("\nThe sources disagree on a point (see Contradictions). Where they are dated, the newer one is given first.\n");
        }
        if !o.age.is_empty() {
            s.push_str(&format!("\n{}\n", o.age));
        }
    }
    if !o.conflicts.is_empty() {
        s.push_str("\n## Contradictions\n\n");
        for c in &o.conflicts {
            s.push_str(&conflict_line(c));
            s.push('\n');
        }
    }
    if !o.sources.is_empty() {
        s.push_str("\n## Sources\n\n");
        for src in &o.sources {
            let when = src.date.as_deref().map(|d| format!(", {}", one_line(d, 40))).unwrap_or_else(|| ", no date".to_string());
            let title = if src.title.is_empty() { String::new() } else { format!(": {}", one_line(&src.title, 160)) };
            s.push_str(&format!("[{}] {}{when}{title}\n    {}\n", src.id, host_of(&src.url), one_line(&src.url, 300)));
        }
    }
    if !o.notes.is_empty() {
        s.push_str("\n## Notes\n\n");
        for n in &o.notes {
            s.push_str(&format!("- {}\n", one_line(n, 400)));
        }
    }
    s
}

/// The report cut to at most `max` characters at the end of a line (the whole of it is in the file).
pub fn capped(report: &str, max: usize) -> String {
    if report.chars().count() <= max {
        return report.to_string();
    }
    let cut: String = report.chars().take(max).collect();
    let at_line = cut.rfind('\n').filter(|&i| i > max / 2).unwrap_or(cut.len());
    format!("{}\n… [cut; the whole report is in the file]\n", cut[..at_line].trim_end())
}

/// The first words of the question as a part of a file name: letters, digits and hyphens only.
pub fn slug(question: &str) -> String {
    let words = super::plan::words_of(&super::plan::clean_question(question));
    let joined: String = words.iter().take(6).map(|w| w.chars().filter(char::is_ascii_alphanumeric).collect::<String>()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join("-");
    let cut: String = joined.chars().take(MAX_SLUG_CHARS).collect();
    let cut = cut.trim_matches('-').to_string();
    if cut.is_empty() {
        "research".to_string()
    } else {
        cut
    }
}

/// The name of a report the program names: the question and the time, `the-latest-rust-release-20261008-143205.md`.
pub fn file_name(question: &str, now: NaiveDateTime) -> String {
    format!("{}-{}.md", slug(question), now.format("%Y%m%d-%H%M%S"))
}

/// The name a person asked for, made into a file name, or why it is refused. A name is letters, digits, spaces, `-`, `_` and `.` inside it: never a folder, a drive or `..`, so a report
/// can only be made in the folder meant for it.
pub fn requested_name(name: &str) -> Result<String, String> {
    let t = name.trim();
    if t.is_empty() || t.chars().count() > MAX_NAME_CHARS {
        return Err(format!("the report name is empty or longer than {MAX_NAME_CHARS} characters"));
    }
    let stem = t.strip_suffix(".md").or_else(|| t.strip_suffix(".MD")).unwrap_or(t).trim();
    if stem.is_empty() || stem.starts_with('.') || stem.ends_with('.') || stem.contains("..") {
        return Err("a report name does not start or end with a dot and has no \"..\" in it".to_string());
    }
    if !stem.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.')) {
        return Err("a report name is letters, digits, spaces, \"-\", \"_\" and \".\": a folder, a drive or a path is not a name".to_string());
    }
    Ok(format!("{}.md", stem.replace(' ', "-")))
}
