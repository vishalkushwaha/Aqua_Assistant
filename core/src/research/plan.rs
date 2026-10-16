//! The plan of a research (card R2-10): which tier a question needs, whether it is about now, and the queries to ask. Pure rules: no model, no network, no clock but the one given.
//!
//! The tiers come from the research behind the card: T0 answer directly, T1 a vertical feed (the weather is the one Aqua has), T2 one search with dated snippets, T3 a search and the
//! text of one or two pages. T4 (a loop in the background) and T5 (cloud research) are not built.

use chrono::{Datelike, NaiveDateTime};

/// The most queries one research asks (a fan-out of rewritten forms of the question).
pub const MAX_QUERIES: usize = 3;
/// The longest query (the search tool takes 300 characters).
pub const MAX_QUERY_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    T0,
    T1,
    T2,
    T3,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::T0 => "T0",
            Tier::T1 => "T1",
            Tier::T2 => "T2",
            Tier::T3 => "T3",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Tier::T0 => "answered directly, no search",
            Tier::T1 => "a live feed, no search",
            Tier::T2 => "a search with dated snippets",
            Tier::T3 => "a search and the text of one or two pages",
        }
    }
}

/// The vertical feeds Aqua has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedKind {
    Weather,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub tier: Tier,
    pub feed: Option<FeedKind>,
    /// The question is about now (a price, a score, "latest"): the newest dated source is preferred.
    pub fresh: bool,
    /// The queries to ask, at most [`MAX_QUERIES`]. Empty for T0.
    pub queries: Vec<String>,
    /// What the computer can say itself (T0): the time or the date.
    pub direct: Option<String>,
    /// One short reason for the tier.
    pub why: &'static str,
}

/// The lower-case words of a text: letters and digits, with apostrophes dropped ("what's" is "whats").
pub fn words_of(text: &str) -> Vec<String> {
    text.to_lowercase().replace(['\'', '\u{2019}'], "").split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

/// Words a person says in front of a question and that are not part of it.
const LEADS: &[&str] = &[
    "hey aqua", "okay aqua", "ok aqua", "aqua", "please", "can you", "could you", "would you", "will you", "i want to know", "i would like to know", "i need to know", "tell me", "find out",
    "do some research on", "do research on", "do a research on", "research", "look up",
];

/// The question with the politeness taken off and without its closing mark: "Please research the latest Rust release?" is "the latest Rust release".
pub fn clean_question(question: &str) -> String {
    let mut s = question.split_whitespace().collect::<Vec<_>>().join(" ");
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(lead) = LEADS.iter().find(|l| lower == **l || lower.strip_prefix(**l).is_some_and(|rest| rest.starts_with([' ', ',', ':']))) else { break };
        let rest = s.get(lead.len()..).unwrap_or("").trim_start_matches([' ', ',', ':']).to_string();
        if rest.is_empty() {
            break;
        }
        s = rest;
    }
    s.trim_end_matches(['?', '.', '!', ' ']).to_string()
}

/// Did the person ask for research in so many words? Then a question that looks like one for chat is still searched.
fn asks_for_research(question: &str) -> bool {
    let padded = format!(" {} ", words_of(question).join(" "));
    ["research", "look up", "lookup", "find out", "search", "sources", "source", "cite", "citation", "citations", "verify", "fact check", "factcheck"].iter().any(|v| padded.contains(&format!(" {v} ")))
}

/// Words that say the answer is about now.
const NOW_WORDS: &[&str] = &["latest", "newest", "current", "currently", "now", "today", "tonight", "breaking", "live"];
const TIME_PHRASES: &[&str] = &["this week", "this month", "this year", "right now", "as of", "last night", "last week", "so far", "who won"];
/// Topics whose answer changes by the hour or the day.
const TOPIC_WORDS: &[&str] = &["price", "prices", "score", "scores", "standings", "news", "headlines", "update", "updates", "trending", "stock", "stocks", "rate", "rates", "weather", "forecast", "tomorrow", "yesterday", "recent", "recently"];

/// Is the answer to this question a thing of now (a price, a score, "the latest")? Then the newest dated source is preferred. A year in the past with nothing that says "now" asks about then.
pub fn is_fresh_sensitive(question: &str, now: NaiveDateTime) -> bool {
    let words = words_of(question);
    let padded = format!(" {} ", words.join(" "));
    let strong = words.iter().any(|w| NOW_WORDS.contains(&w.as_str())) || TIME_PHRASES.iter().any(|p| padded.contains(&format!(" {p} ")));
    let topic = words.iter().any(|w| TOPIC_WORDS.contains(&w.as_str()));
    if !(strong || topic) {
        return false;
    }
    let past_year = words.iter().filter_map(|w| w.parse::<i32>().ok()).any(|y| (1900..now.year()).contains(&y));
    !(past_year && !strong)
}

/// Words that ask for a comparison, an overview or a summary: the snippets of a search seldom hold such an answer, so pages are read (at depth 3).
const WIDE_WORDS: &[&str] = &[
    "compare", "comparison", "versus", "vs", "overview", "analysis", "analyse", "analyze", "pros", "cons", "difference", "differences", "advantages", "disadvantages", "detailed", "thorough", "thoroughly",
    "summary", "summarize", "summarise", "report", "review", "timeline", "background", "history",
];

fn needs_detail(question: &str) -> bool {
    let words = words_of(question);
    words.len() >= 14 || words.iter().any(|w| WIDE_WORDS.contains(&w.as_str())) || !crate::sources::urls_in(question).is_empty()
}

/// A question that asks for an explanation or a meaning: it needs no source. (A person who asked for research, sources or a check is not asked this.)
fn is_conceptual(cleaned: &str) -> bool {
    let words = words_of(cleaned);
    let padded = format!(" {} ", words.join(" "));
    let first = words.first().map(String::as_str).unwrap_or("");
    ["explain", "define", "why"].contains(&first)
        || ["what is a ", "what is an ", "meaning of ", "definition of ", "stands for ", "stand for "].iter().any(|p| padded.contains(&format!(" {p}")))
        || ((padded.contains(" how does ") || padded.contains(" how do ")) && words.iter().any(|w| w == "work" || w == "works"))
}

/// A sum: digits and operators, perhaps after "what is" or "calculate".
fn is_arithmetic(cleaned: &str) -> bool {
    let lower = cleaned.to_lowercase();
    let rest = ["what is ", "whats ", "what's ", "calculate ", "compute ", "how much is "].iter().find_map(|p| lower.strip_prefix(p)).unwrap_or(lower.as_str());
    let has_digit = rest.chars().any(|c| c.is_ascii_digit());
    let has_operator = rest.chars().any(|c| matches!(c, '+' | '*' | '/' | '×' | '÷' | '^' | '%')) || rest.contains(" - ");
    has_digit && has_operator && rest.chars().all(|c| c.is_ascii_digit() || " +-*/x×÷^%().,".contains(c))
}

// ---- the queries ------------------------------------------------------------------------------------------------------------------

/// Words that carry nothing in a search query.
const KEYWORD_STOP: &[&str] = &[
    "what", "whats", "which", "who", "whom", "whose", "when", "where", "why", "how", "is", "are", "was", "were", "be", "been", "am", "do", "does", "did", "the", "a", "an", "of", "to", "in", "on", "for", "with",
    "and", "or", "about", "me", "my", "i", "you", "your", "can", "could", "would", "will", "should", "tell", "please", "it", "its", "there", "this", "that", "these", "those", "has", "have", "had", "give", "show",
    "find", "explain",
];

/// The words of a question that matter to a search, in order, at most eight.
fn keywords(cleaned: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in words_of(cleaned) {
        if KEYWORD_STOP.contains(&w.as_str()) || out.contains(&w) {
            continue;
        }
        out.push(w);
        if out.len() >= 8 {
            break;
        }
    }
    out
}

/// Cut a query to the longest the search takes, at a word.
fn cut_query(q: &str) -> String {
    if q.chars().count() <= MAX_QUERY_CHARS {
        return q.to_string();
    }
    let cut: String = q.chars().take(MAX_QUERY_CHARS).collect();
    match cut.rfind(' ') {
        Some(i) if i > MAX_QUERY_CHARS / 2 => cut[..i].to_string(),
        _ => cut,
    }
}

/// Adds a query unless it is empty, short, already there (in any case), or the list is full.
fn add(out: &mut Vec<String>, q: &str) {
    let q = cut_query(&q.split_whitespace().collect::<Vec<_>>().join(" "));
    let key = q.to_lowercase();
    if q.chars().count() >= 2 && out.len() < MAX_QUERIES && !out.iter().any(|x| x.to_lowercase() == key) {
        out.push(q);
    }
}

/// The opening words of a comparison that are not part of the first thing compared.
fn strip_opening(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    for opener in ["what is the difference between ", "what are the differences between ", "difference between ", "differences between ", "compare ", "comparison of "] {
        if lower.starts_with(opener) {
            return text[opener.len()..].to_string();
        }
    }
    text.to_string()
}

/// The two things a question compares ("A vs B", "compare A and B", "the difference between A and B").
fn comparison_sides(cleaned: &str) -> Option<(String, String)> {
    let lower = cleaned.to_ascii_lowercase();
    let tidy = |s: &str| s.trim().trim_matches(|c: char| c == ',' || c == '.').to_string();
    for sep in [" versus ", " vs. ", " vs ", " compared to ", " compared with "] {
        if let Some(i) = lower.find(sep) {
            let (left, right) = (tidy(&strip_opening(&cleaned[..i])), tidy(&cleaned[i + sep.len()..]));
            if left.chars().count() >= 2 && right.chars().count() >= 2 {
                return Some((left, right));
            }
        }
    }
    for opener in ["difference between ", "differences between ", "compare "] {
        if let Some(i) = lower.find(opener) {
            let rest = &cleaned[i + opener.len()..];
            if let Some(j) = rest.to_ascii_lowercase().find(" and ") {
                let (left, right) = (tidy(&rest[..j]), tidy(&rest[j + 5..]));
                if left.chars().count() >= 2 && right.chars().count() >= 2 {
                    return Some((left, right));
                }
            }
        }
    }
    None
}

/// The first phrase the person put in quotation marks.
fn quoted_phrase(text: &str) -> Option<String> {
    let mut chars = text.char_indices().filter(|(_, c)| matches!(c, '"' | '\u{201c}' | '\u{201d}'));
    let (start, open) = chars.next()?;
    let (end, _) = chars.next()?;
    let phrase = text[start + open.len_utf8()..end].trim().to_string();
    (phrase.chars().count() >= 3).then_some(phrase)
}

/// Names in the question: words written with a capital that are not its first word.
fn capitalised_names(text: &str) -> Vec<String> {
    text.split_whitespace()
        .skip(1)
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| w.chars().count() >= 2 && w.chars().next().is_some_and(char::is_uppercase))
        .map(str::to_string)
        .collect()
}

/// The rewritten forms of a question (a fan-out), at most [`MAX_QUERIES`]: the question as it is; its keywords; and a third that depends on the question. A question about now gets the
/// keywords with the year (or "latest" when a year is already there); a comparison gets each side on its own; otherwise a phrase in quotation marks, or the names in it. Made by rules,
/// with no model.
pub fn queries_for(cleaned: &str, fresh: bool, year: i32) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    add(&mut out, cleaned);
    if let Some((left, right)) = comparison_sides(cleaned) {
        add(&mut out, &left);
        add(&mut out, &right);
        return out;
    }
    let kw = keywords(cleaned);
    if kw.len() >= 2 {
        add(&mut out, &kw.join(" "));
    }
    if fresh {
        if !kw.is_empty() {
            let has_year = kw.iter().any(|w| w.len() == 4 && w.parse::<i32>().is_ok_and(|y| (1900..=2100).contains(&y)));
            add(&mut out, &if has_year { format!("{} latest", kw.join(" ")) } else { format!("{} {year}", kw.join(" ")) });
        }
    } else if let Some(phrase) = quoted_phrase(cleaned) {
        add(&mut out, &phrase);
    } else {
        let names = capitalised_names(cleaned);
        if names.len() >= 2 {
            add(&mut out, &names.join(" "));
        }
    }
    out
}

// ---- the plan ---------------------------------------------------------------------------------------------------------------------

/// The plan for a question. `depth` is the deepest tier allowed: 2 (a search) or 3 (a search and pages); anything else is held to that range. `now` is the clock.
pub fn plan(question: &str, depth: u8, now: NaiveDateTime) -> Plan {
    let depth = depth.clamp(2, 3);
    let cleaned = clean_question(question);
    let t0 = |direct: Option<String>, why: &'static str| Plan { tier: Tier::T0, feed: None, fresh: false, queries: Vec::new(), direct, why };
    if cleaned.is_empty() {
        return t0(None, "there is no question");
    }
    if let Some(text) = crate::local_facts::handle_request_at(question, now) {
        return t0(Some(text), "this computer's clock answers it");
    }
    let explicit = asks_for_research(question);
    let fresh = is_fresh_sensitive(&cleaned, now);
    let queries = queries_for(&cleaned, fresh, now.year());
    if !explicit && is_arithmetic(&cleaned) {
        return t0(None, "it is a sum");
    }
    if !explicit && !fresh && is_conceptual(&cleaned) {
        return t0(None, "it asks for an explanation, which needs no source");
    }
    if crate::agent_tools::asks_about_weather(&cleaned) && crate::live_weather::ask_of(&cleaned).is_some() {
        return Plan { tier: Tier::T1, feed: Some(FeedKind::Weather), fresh: true, queries, direct: None, why: "the weather has a live feed of its own" };
    }
    if depth >= 3 && needs_detail(question) {
        return Plan { tier: Tier::T3, feed: None, fresh, queries, direct: None, why: "it asks for detail, so pages are read as well" };
    }
    let why = if fresh { "it is about now: a search with dated snippets" } else { "a search with snippets is the place to start" };
    Plan { tier: Tier::T2, feed: None, fresh, queries, direct: None, why }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 8).unwrap().and_hms_opt(14, 32, 5).unwrap()
    }

    /// Ten and more example questions, each with the tier the rules give at depth 3.
    #[test]
    fn the_tier_follows_from_the_question() {
        let cases: &[(&str, Tier)] = &[
            ("What time is it?", Tier::T0),
            ("Explain how photosynthesis works", Tier::T0),
            ("Why is the sky blue?", Tier::T0),
            ("What is 15 * 12 + 3?", Tier::T0),
            ("   ", Tier::T0),
            ("What's the weather in Pune?", Tier::T1),
            ("What is the latest stable version of Rust?", Tier::T2),
            ("What is the current price of gold per ounce?", Tier::T2),
            ("latest news about the Mars mission", Tier::T2),
            ("Who founded Acme Corporation?", Tier::T2),
            ("What was the price of bitcoin in 2017?", Tier::T2),
            ("Compare the Rust and Go memory models and explain which suits embedded systems", Tier::T3),
            ("Research the history of the Pune metro and summarize the main delays", Tier::T3),
            ("Research why the sky is blue", Tier::T2),
        ];
        for (question, tier) in cases {
            assert_eq!(plan(question, 3, now()).tier, *tier, "{question:?}");
        }
        // The clock answers a question about the time itself.
        let p = plan("What time is it?", 3, now());
        assert!(p.direct.as_deref().is_some_and(|d| d.starts_with("It's ")) && p.queries.is_empty(), "{p:?}");
        // At depth 2 no tier reaches the pages.
        assert_eq!(plan("Compare the Rust and Go memory models and explain which suits embedded systems", 2, now()).tier, Tier::T2);
        // A depth outside the range is held to it.
        assert_eq!(plan("Compare Rust vs Go", 9, now()).tier, Tier::T3);
        assert_eq!(plan("Compare Rust vs Go", 0, now()).tier, Tier::T2);
    }

    #[test]
    fn a_question_about_now_prefers_the_newest_source_and_a_past_year_does_not() {
        for q in ["What is the latest stable version of Rust?", "bitcoin price", "who won the match last night", "What's the weather in Pune?", "stock news today"] {
            assert!(is_fresh_sensitive(q, now()), "{q}");
        }
        for q in ["Who founded Acme Corporation?", "What was the price of bitcoin in 2017?", "history of the Roman empire", "What is the population of Pune?"] {
            assert!(!is_fresh_sensitive(q, now()), "{q}");
        }
        // A past year next to a word that says "now" is still about now.
        assert!(is_fresh_sensitive("what is the current price compared with 2017", now()));
        assert!(plan("What is the latest stable version of Rust?", 2, now()).fresh);
        assert!(!plan("Who founded Acme Corporation?", 2, now()).fresh);
    }

    #[test]
    fn the_fan_out_is_at_most_three_queries_made_by_rules() {
        let q = queries_for("What is the latest stable version of Rust", true, 2026);
        assert_eq!(q, vec!["What is the latest stable version of Rust", "latest stable version rust", "latest stable version rust 2026"]);
        // A year that is already in the question is not added twice.
        let q = queries_for("latest Rust release in 2025", true, 2026);
        assert_eq!(q[2], "latest rust release 2025 latest");
        // A comparison is asked as a whole and as each side.
        let q = queries_for("Compare Rust vs Go", false, 2026);
        assert_eq!(q, vec!["Compare Rust vs Go", "Rust", "Go"]);
        let q = queries_for("What is the difference between a mutex and a semaphore", false, 2026);
        assert_eq!(q, vec!["What is the difference between a mutex and a semaphore", "a mutex", "a semaphore"]);
        // A quoted phrase, or the names in the question, make the third query.
        let q = queries_for("Who said \"the medium is the message\" first", false, 2026);
        assert_eq!(q[2], "the medium is the message");
        let q = queries_for("When did Satya Nadella join Microsoft", false, 2026);
        assert_eq!(q[2], "Satya Nadella Microsoft");
        // Whatever the question, never more than three, none empty, none repeated, none longer than the search takes.
        let long = "word ".repeat(200);
        for question in ["a", "Rust", "Compare A vs B vs C", "latest latest latest", long.as_str(), "What is the latest stable version of Rust?"] {
            for fresh in [false, true] {
                let q = queries_for(&clean_question(question), fresh, 2026);
                assert!(q.len() <= MAX_QUERIES, "{question:?}: {q:?}");
                assert!(q.iter().all(|x| !x.trim().is_empty() && x.chars().count() <= MAX_QUERY_CHARS), "{q:?}");
                let mut lower: Vec<String> = q.iter().map(|x| x.to_lowercase()).collect();
                lower.dedup();
                assert_eq!(lower.len(), q.len(), "{q:?}");
            }
        }
        assert_eq!(plan("Compare Rust vs Go", 3, now()).queries.len(), 3);
    }

    #[test]
    fn politeness_and_the_closing_mark_are_not_part_of_the_question() {
        assert_eq!(clean_question("  Please, research   the latest Rust release? "), "the latest Rust release");
        assert_eq!(clean_question("Hey Aqua, can you tell me what time it is!"), "what time it is");
        assert_eq!(clean_question("research"), "research", "a lead that is all there is stays");
        assert_eq!(clean_question(""), "");
    }
}
