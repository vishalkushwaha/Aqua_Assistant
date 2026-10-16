//! What a person asked for, read out of their words: which platforms, whether to research, whether to
//! make an image, whether to ask before publishing, when to run, and about what.
//!
//! This is deliberately plain pattern matching. It understands what it can understand exactly, and
//! what it does not understand becomes a question, never a guess.

use crate::flows::platform::{self, Platform};
use crate::flows::schedule_text::{self, ScheduleSpec};
use chrono::NaiveDateTime;

#[derive(Debug, Clone)]
pub struct Intent {
    pub platforms: Vec<&'static Platform>,
    pub research: bool,
    pub image: bool,
    pub publish: bool,
    /// The person said not to ask before publishing.
    pub no_approval: bool,
    /// Save the result to a spreadsheet, said outright.
    pub excel: bool,
    /// The request is about reading posts *from* a spreadsheet (a later phase).
    pub excel_source: bool,
    pub schedule: Option<ScheduleSpec>,
    /// A schedule was asked for but could not be read exactly.
    pub schedule_unclear: bool,
    pub topic: Option<String>,
    pub tone: Option<String>,
}

pub fn words_lower(text: &str) -> Vec<String> {
    // "don't" is one word ("dont"), not "don" and "t".
    let text = text.replace(['\'', '\u{2019}'], "");
    text.to_ascii_lowercase().split(|c: char| !c.is_alphanumeric() && c != '-').filter(|w| !w.is_empty()).map(String::from).collect()
}

fn has_any(words: &[String], any: &[&str]) -> bool {
    words.iter().any(|w| any.contains(&w.as_str()))
}

/// The topic: what follows "about" (or "on the topic of"), up to where the sentence moves on.
pub fn extract_topic(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let markers = [" about ", " on the topic of ", " on the topic ", " topic: ", " regarding ", " covering "];
    let (start, mlen) = markers.iter().filter_map(|m| lower.find(m).map(|i| (i, m.len()))).min_by_key(|(i, _)| *i)?;
    let rest = &text[start + mlen..];
    let mut end = rest.len();
    for (i, c) in rest.char_indices() {
        if matches!(c, '.' | ',' | ';' | '!' | '?' | '\n') {
            end = i;
            break;
        }
    }
    let rest = &rest[..end];
    // Cut where the sentence turns to something else: "... and publish it", "... on LinkedIn".
    let mut cut = rest.len();
    let ws: Vec<(usize, &str)> = {
        let mut v = Vec::new();
        let mut s = None;
        for (i, c) in rest.char_indices() {
            if c.is_whitespace() {
                if let Some(st) = s.take() {
                    v.push((st, &rest[st..i]));
                }
            } else if s.is_none() {
                s = Some(i);
            }
        }
        if let Some(st) = s {
            v.push((st, &rest[st..]));
        }
        v
    };
    for (k, (pos, w)) in ws.iter().enumerate() {
        let lw = w.to_ascii_lowercase();
        let next_is_platform = ws.get(k + 1).is_some_and(|(_, n)| !platform::mentioned(n).is_empty());
        if matches!(lw.as_str(), "and" | "then" | "with") || (matches!(lw.as_str(), "on" | "to" | "for" | "in" | "at" | "via") && next_is_platform) {
            cut = *pos;
            break;
        }
    }
    let mut topic = rest[..cut].trim().trim_matches(|c| c == '"' || c == '\'' || c == '“' || c == '”').trim().to_string();
    for prefix in ["the topic of ", "the topic ", "topic "] {
        if topic.to_ascii_lowercase().starts_with(prefix) {
            topic = topic[prefix.len()..].trim().to_string();
        }
    }
    let generic = ["it", "this", "that", "them", "these", "those", "something", "anything", "the topic", ""];
    if generic.contains(&topic.to_ascii_lowercase().as_str()) || topic.chars().count() > 200 {
        return None;
    }
    Some(topic)
}

pub fn parse_create(text: &str, now: NaiveDateTime) -> Intent {
    let w = words_lower(text);
    let joined = w.join(" ");
    let negated_image = joined.contains("no image") || joined.contains("without an image") || joined.contains("without image") || joined.contains("text only") || joined.contains("text-only") || joined.contains("without a picture");
    let image = !negated_image && has_any(&w, &["image", "images", "picture", "photo", "illustration", "graphic", "visual"]);
    let research = has_any(&w, &["research", "latest", "trend", "trends", "news", "current", "recent"]) || joined.contains("find out") || joined.contains("look up");
    let publish = has_any(&w, &["publish", "post", "share", "upload", "tweet", "posting"]);
    let no_approval = joined.contains("without asking") || joined.contains("dont ask") || joined.contains("don-t ask") || joined.contains("no approval") || joined.contains("never ask") || joined.contains("without approval");
    let excel = has_any(&w, &["excel", "spreadsheet", "xlsx"]) && has_any(&w, &["save", "store", "log", "record", "export", "write", "add", "keep"]);
    let excel_source = has_any(&w, &["excel", "spreadsheet", "xlsx"]) && (joined.contains("from excel") || joined.contains("from the excel") || joined.contains("from my excel") || joined.contains("from the spreadsheet") || joined.contains("each row") || joined.contains("every row") || joined.contains("all rows") || joined.contains("everything from"));
    let (schedule, schedule_unclear) = if schedule_text::mentions_schedule(text) {
        match schedule_text::parse(text, now) {
            Some(s) => (Some(s), false),
            None => (None, true),
        }
    } else {
        (None, false)
    };
    let tone = ["professional", "friendly", "casual", "technical", "persuasive"].iter().find(|t| w.iter().any(|x| x == *t)).map(|t| t.to_string());
    Intent { platforms: platform::mentioned(text).into_iter().map(|(_, p)| p).collect(), research, image, publish, no_approval, excel: excel && !excel_source, excel_source, schedule, schedule_unclear, topic: extract_topic(text), tone }
}
