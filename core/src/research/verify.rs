//! Cross-checking what the sources said (card R2-10): two sources that give different numbers or dates for the same thing, and a question that no source answers. No model.
//!
//! The rules are narrow on purpose, so that what they report is true. Two sentences are about the same thing only when they share at least two of their own words, the same version
//! numbers and the same years; a number is compared only when it has a unit of a known kind (`sources.rs` knows them: a percentage, a scale word, a currency, degrees, a distance, a
//! time), and only with a number of the same kind. The unit trap is closed the way `sources.rs` closes it: `5 million` and `5 billion` are different quantities, and `2.5 million` and
//! `2,500,000` are the same. A number with no unit (a count of apples, the number of a version) is never called a contradiction.

use super::rank::{content_terms, stem, stems_of};
use crate::bm25::tokens;
use crate::sources::numbers_in;
use chrono::NaiveDate;
use std::collections::BTreeSet;

/// The most contradictions one report lists.
pub const MAX_CONTRADICTIONS: usize = 5;

/// A sentence that was ranked high for the question, with the date of its source.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub source: usize,
    pub text: String,
    pub date: Option<NaiveDate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Number,
    Date,
}

/// Two candidates (by index into the slice that was checked) that disagree. `newer` is the one from the source with the later date, when both have a date and the dates differ.
#[derive(Debug, Clone, PartialEq)]
pub struct Contradiction {
    pub kind: Kind,
    pub first: usize,
    pub second: usize,
    pub newer: Option<usize>,
}

// ---- dates written in a sentence ----------------------------------------------------------------------------------------------------

/// A date as a sentence states it: a whole day, or a month of a year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stated {
    Day(NaiveDate),
    Month(i32, u32),
}

fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                spans.push((s, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }
    spans
}

fn trim_word(w: &str) -> &str {
    w.trim_matches(|c: char| matches!(c, ',' | '.' | ';' | ':' | '(' | ')' | '"' | '\'' | '[' | ']'))
}

/// The month a word names (written with a capital: "May" the month, not "may" the verb).
fn month_of(w: &str) -> Option<u32> {
    let w = trim_word(w);
    if !w.chars().next().is_some_and(char::is_uppercase) {
        return None;
    }
    let l = w.to_lowercase();
    const NAMES: [&str; 12] = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
    NAMES.iter().position(|n| l == *n || (l.len() == 3 && n.starts_with(l.as_str())) || (l == "sept" && *n == "september")).map(|i| i as u32 + 1)
}

fn year_of(w: &str) -> Option<i32> {
    let w = trim_word(w);
    if w.len() != 4 || !w.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    w.parse::<i32>().ok().filter(|y| (1900..=2100).contains(y))
}

fn day_of(w: &str) -> Option<u32> {
    let w = trim_word(w).to_lowercase();
    let digits = w.trim_end_matches("st").trim_end_matches("nd").trim_end_matches("rd").trim_end_matches("th");
    if digits.is_empty() || digits.len() > 2 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok().filter(|d| (1..=31).contains(d))
}

fn iso_date(w: &str) -> Option<NaiveDate> {
    if w.len() == 10 {
        NaiveDate::parse_from_str(w, "%Y-%m-%d").ok()
    } else {
        None
    }
}

/// The dates a sentence states, and the sentence with them blanked out (so that the day and the year of a date are not taken for numbers of their own).
/// Forms: `2025-09-14`, `14 September 2025`, `September 14, 2025`, `September 2025`.
pub fn find_dates(text: &str) -> (Vec<Stated>, String) {
    let spans = word_spans(text);
    let word = |i: usize| spans.get(i).map(|&(s, e)| &text[s..e]);
    let mut found: Vec<Stated> = Vec::new();
    let mut blanks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < spans.len() {
        let w0 = word(i).unwrap_or("");
        let (w1, w2) = (word(i + 1), word(i + 2));
        let (stated, used) = if let Some(d) = iso_date(trim_word(w0)) {
            (Some(Stated::Day(d)), 1)
        } else if let (Some(d), Some(m), Some(y)) = (day_of(w0), w1.and_then(month_of), w2.and_then(year_of)) {
            (NaiveDate::from_ymd_opt(y, m, d).map(Stated::Day), 3)
        } else if let (Some(m), Some(d), Some(y)) = (month_of(w0), w1.and_then(day_of), w2.and_then(year_of)) {
            (NaiveDate::from_ymd_opt(y, m, d).map(Stated::Day), 3)
        } else if let (Some(m), Some(y)) = (month_of(w0), w1.and_then(year_of)) {
            (Some(Stated::Month(y, m)), 2)
        } else {
            (None, 1)
        };
        match stated {
            Some(s) => {
                found.push(s);
                blanks.push((spans[i].0, spans[i + used - 1].1));
                i += used;
            }
            None => i += 1,
        }
    }
    let mut blanked = String::new();
    let mut last = 0;
    for (s, e) in blanks {
        blanked.push_str(&text[last..s]);
        blanked.push(' ');
        last = e;
    }
    blanked.push_str(&text[last..]);
    (found, blanked)
}

// ---- what two sentences are about ----------------------------------------------------------------------------------------------------

/// Words that name a unit, a month, or only fill a sentence: they do not say what it is about.
const FILLER: &[&str] = &[
    "percent", "pct", "thousand", "million", "billion", "trillion", "dollar", "dollars", "usd", "euro", "euros", "eur", "rupee", "rupees", "inr", "celsius", "fahrenheit", "degrees", "degree", "km", "kilometer",
    "kilometers", "kilometre", "kilometres", "mile", "miles", "kg", "kilogram", "kilograms", "lb", "lbs", "kb", "mb", "gb", "tb", "second", "seconds", "minute", "minutes", "hour", "hours", "day", "days", "week",
    "weeks", "month", "months", "year", "years", "january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december", "was", "were", "been", "has", "had",
    "have", "its", "their", "from", "with", "which", "will", "would", "about", "approximately", "around", "roughly", "than", "into", "over", "under", "also", "just", "only", "some", "such", "not", "but", "per",
    "reported", "reports", "report", "according", "says", "said", "about",
];

fn is_year(n: &str) -> bool {
    n.len() == 4 && n.parse::<i32>().is_ok_and(|y| (1900..=2100).contains(&y))
}

/// What a sentence is about: its own words (stems, without units, months and filler), the numbers that name a thing (a version, a model number) and the years it mentions.
struct Subject {
    words: BTreeSet<String>,
    names: Vec<String>,
    years: BTreeSet<String>,
}

fn subject_of(blanked: &str) -> Subject {
    let words = tokens(blanked).into_iter().filter(|t| !t.chars().all(|c| c.is_ascii_digit()) && !FILLER.contains(&t.as_str())).map(|t| stem(&t)).collect();
    let (_, bare) = numbers_in(blanked);
    let mut names: Vec<String> = bare.iter().filter(|n| !is_year(n)).cloned().collect();
    names.sort();
    names.dedup();
    let years = bare.into_iter().filter(|n| is_year(n)).collect();
    Subject { words, names, years }
}

/// Are two sentences about the same thing? At least two words in common, and most of the shorter one's words; the same names of versions; and no two different years.
fn same_subject(a: &Subject, b: &Subject) -> bool {
    let shared = a.words.intersection(&b.words).count();
    let smaller = a.words.len().min(b.words.len());
    if shared < 2 || shared * 10 < smaller * 6 {
        return false;
    }
    if a.names != b.names {
        return false;
    }
    a.years.is_empty() || b.years.is_empty() || a.years.intersection(&b.years).next().is_some()
}

/// Do these two lists of quantities give a different value for one kind of quantity, with no value in common for that kind?
fn quantities_differ(a: &[crate::sources::Quantity], b: &[crate::sources::Quantity]) -> bool {
    let families: BTreeSet<&str> = a.iter().map(|q| q.family).collect();
    families.into_iter().any(|family| {
        let va: BTreeSet<&str> = a.iter().filter(|q| q.family == family).map(|q| q.value.as_str()).collect();
        let vb: BTreeSet<&str> = b.iter().filter(|q| q.family == family).map(|q| q.value.as_str()).collect();
        !vb.is_empty() && va.is_disjoint(&vb)
    })
}

/// Do these two lists of dates give different dates of one kind (days, or months), with none in common?
fn dates_differ(a: &[Stated], b: &[Stated]) -> bool {
    let days = |v: &[Stated]| v.iter().filter_map(|s| if let Stated::Day(d) = s { Some(*d) } else { None }).collect::<BTreeSet<NaiveDate>>();
    let months = |v: &[Stated]| v.iter().filter_map(|s| if let Stated::Month(y, m) = s { Some((*y, *m)) } else { None }).collect::<BTreeSet<(i32, u32)>>();
    let (da, db, ma, mb) = (days(a), days(b), months(a), months(b));
    (!da.is_empty() && !db.is_empty() && da.is_disjoint(&db)) || (!ma.is_empty() && !mb.is_empty() && ma.is_disjoint(&mb))
}

/// The pairs of candidates from different sources that give different numbers, or different dates, for the same thing. A pair of sources is listed once for each kind.
pub fn contradictions(candidates: &[Candidate]) -> Vec<Contradiction> {
    struct Prepared {
        subject: Subject,
        quantities: Vec<crate::sources::Quantity>,
        dates: Vec<Stated>,
    }
    let prepared: Vec<Prepared> = candidates
        .iter()
        .map(|c| {
            let (dates, blanked) = find_dates(&c.text);
            let (quantities, _) = numbers_in(&blanked);
            Prepared { subject: subject_of(&blanked), quantities, dates }
        })
        .collect();
    let mut out: Vec<Contradiction> = Vec::new();
    for i in 0..candidates.len() {
        for j in (i + 1)..candidates.len() {
            if candidates[i].source == candidates[j].source || !same_subject(&prepared[i].subject, &prepared[j].subject) {
                continue;
            }
            let kind = if quantities_differ(&prepared[i].quantities, &prepared[j].quantities) {
                Kind::Number
            } else if dates_differ(&prepared[i].dates, &prepared[j].dates) {
                Kind::Date
            } else {
                continue;
            };
            let pair = |c: &Contradiction| {
                let (a, b) = (candidates[c.first].source, candidates[c.second].source);
                (a.min(b), a.max(b), c.kind)
            };
            let this = Contradiction { kind, first: i, second: j, newer: newer_of(candidates, i, j) };
            if out.iter().any(|c| pair(c) == pair(&this)) {
                continue;
            }
            out.push(this);
            if out.len() >= MAX_CONTRADICTIONS {
                return out;
            }
        }
    }
    out
}

/// The candidate whose source is dated later, when both are dated and the dates differ.
fn newer_of(candidates: &[Candidate], i: usize, j: usize) -> Option<usize> {
    match (candidates[i].date, candidates[j].date) {
        (Some(a), Some(b)) if a > b => Some(i),
        (Some(a), Some(b)) if b > a => Some(j),
        _ => None,
    }
}

// ---- a question nothing answers --------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answerability {
    Answered,
    /// No source says it; the reason is one plain sentence for the report.
    Unanswered(&'static str),
}

/// Does the question ask for a figure (a count, an amount, a rate)?
fn asks_for_number(question: &str) -> bool {
    let lower = question.to_lowercase();
    ["how many", "how much", "population of", "percent", "percentage", "price of", "cost of", "how old", "how tall", "how far", "how long", "revenue", "salary", "number of"].iter().any(|p| lower.contains(p))
}

/// Does the question ask for a date or a year?
fn asks_for_date(question: &str) -> bool {
    let lower = question.to_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    first == "when" || ["what year", "what date", "which year", "release date", "date of"].iter().any(|p| lower.contains(p))
}

fn states_a_date(sentence: &str) -> bool {
    !find_dates(sentence).0.is_empty() || sentence.split_whitespace().any(|w| year_of(w).is_some())
}

/// Do the best passages answer the question? `best` is the top few as (sentence, the title of its source). A passage answers when it holds the question's own words (at least two of
/// them when there are two or more, and 60 percent), and, for a question that asks for a figure or a date, when it states one. If none does, the question is not answered: the
/// report says so and gives no answer of its own.
pub fn answerability(question: &str, best: &[(&str, &str)]) -> Answerability {
    if best.is_empty() {
        return Answerability::Unanswered("no source I read says anything about it");
    }
    let terms = content_terms(question);
    let (needs_number, needs_date) = (asks_for_number(question), asks_for_date(question));
    let mut topic_found = false;
    for (sentence, title) in best {
        let have = stems_of(&format!("{sentence} {title}"));
        let matched = terms.iter().filter(|t| have.contains(*t)).count();
        let covers = terms.is_empty() || (matched >= terms.len().min(2) && matched * 10 >= terms.len() * 6);
        if !covers {
            continue;
        }
        topic_found = true;
        if needs_number && !sentence.chars().any(|c| c.is_ascii_digit()) {
            continue;
        }
        if needs_date && !states_a_date(sentence) {
            continue;
        }
        return Answerability::Answered;
    }
    if topic_found && needs_number {
        Answerability::Unanswered("the sources are about it, but none of them states the figure that was asked for")
    } else if topic_found && needs_date {
        Answerability::Unanswered("the sources are about it, but none of them states the date that was asked for")
    } else {
        Answerability::Unanswered("no source I read says it")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn cand(source: usize, text: &str, date: Option<NaiveDate>) -> Candidate {
        Candidate { source, text: text.to_string(), date }
    }

    /// Two sources give a different number for the same thing: the newer dated one is named.
    #[test]
    fn two_sources_with_different_numbers_for_the_same_thing_are_found_and_the_newer_one_is_named() {
        let older = cand(0, "Acme revenue for 2024 was 4.8 billion dollars.", Some(day(2025, 1, 10)));
        let newer = cand(1, "Acme revenue for 2024 was 5 billion dollars.", Some(day(2025, 3, 1)));
        let found = contradictions(&[older.clone(), newer.clone()]);
        assert_eq!(found, vec![Contradiction { kind: Kind::Number, first: 0, second: 1, newer: Some(1) }]);
        // The order they come in makes no difference to which is newer.
        let found = contradictions(&[newer.clone(), older.clone()]);
        assert_eq!(found, vec![Contradiction { kind: Kind::Number, first: 0, second: 1, newer: Some(0) }]);
        // The same date, or a missing date: they disagree and neither is preferred.
        let same = contradictions(&[cand(0, &older.text, Some(day(2025, 3, 1))), newer.clone()]);
        assert_eq!(same[0].newer, None);
        let undated = contradictions(&[cand(0, &older.text, None), newer.clone()]);
        assert_eq!((undated.len(), undated[0].newer), (1, None));
    }

    /// The unit trap: 5 million is not 5 billion, and the same quantity written two ways is one quantity.
    #[test]
    fn million_is_not_billion_and_the_same_quantity_written_another_way_is_not_a_contradiction() {
        let m = cand(0, "Acme revenue for 2024 was 5 million dollars.", Some(day(2025, 1, 10)));
        let b = cand(1, "Acme revenue for 2024 was 5 billion dollars.", Some(day(2025, 3, 1)));
        let found = contradictions(&[m, b]);
        assert_eq!((found.len(), found[0].kind, found[0].newer), (1, Kind::Number, Some(1)), "the digits are the same and the quantities are not");
        // 2.5 million and 2,500,000 dollars: one quantity.
        let a = cand(0, "The Acme grant for the school was 2.5 million dollars.", None);
        let b = cand(1, "The Acme grant for the school was worth 2,500,000 dollars.", None);
        assert!(contradictions(&[a, b]).is_empty());
        // 25.2 billion and 25,200 million: one quantity.
        let a = cand(0, "Acme revenue for the year was 25.2 billion dollars.", None);
        let b = cand(1, "Acme revenue for the year was 25,200 million dollars.", None);
        assert!(contradictions(&[a, b]).is_empty());
        // A percentage is not an amount, and a count with no unit is not compared.
        let a = cand(0, "Acme revenue for the year grew 5 percent.", None);
        let b = cand(1, "Acme revenue for the year was 5 billion dollars.", None);
        assert!(contradictions(&[a, b]).is_empty(), "different kinds of quantity are never compared");
        let a = cand(0, "Acme revenue for the year came from 12 stores.", None);
        let b = cand(1, "Acme revenue for the year came from 40 stores.", None);
        assert!(contradictions(&[a, b]).is_empty(), "a number with no unit of a known kind is a count of things and not a measure");
    }

    #[test]
    fn two_dates_for_the_same_event_are_found_and_a_different_event_or_a_different_year_is_not() {
        let a = cand(0, "Rust 1.90.0 was released on 14 September 2025.", Some(day(2025, 9, 20)));
        let b = cand(1, "Rust 1.90.0 was released on 18 September 2025.", Some(day(2025, 10, 2)));
        let found = contradictions(&[a.clone(), b.clone()]);
        assert_eq!((found.len(), found[0].kind, found[0].newer), (1, Kind::Date, Some(1)));
        // The same date written two ways is one date.
        let c = cand(2, "Rust 1.90.0 was released on September 14, 2025.", None);
        assert!(contradictions(&[a.clone(), c]).is_empty());
        // Another version is another event.
        let d = cand(3, "Rust 1.89.0 was released on 7 August 2025.", None);
        assert!(contradictions(&[a.clone(), d]).is_empty(), "a version number names the thing");
        // Two sentences from one source are not two sources.
        let e = cand(0, "Rust 1.90.0 was released on 18 September 2025.", None);
        assert!(contradictions(&[a.clone(), e]).is_empty());
        // Sentences that are about different things share too few words.
        let f = cand(4, "Pune airport was opened on 18 September 2025.", None);
        assert!(contradictions(&[a, f]).is_empty());
        // Different years: different things.
        let g = cand(0, "Acme revenue for 2023 was 4 billion dollars.", None);
        let h = cand(1, "Acme revenue for 2024 was 5 billion dollars.", None);
        assert!(contradictions(&[g, h]).is_empty());
    }

    #[test]
    fn the_same_pair_of_sources_is_listed_once_and_the_list_has_a_limit() {
        let a1 = cand(0, "Acme revenue for 2024 was 4 billion dollars.", None);
        let a2 = cand(0, "Acme reported revenue for 2024 near 4 billion dollars.", None);
        let b = cand(1, "Acme revenue for 2024 was 5 billion dollars.", None);
        assert_eq!(contradictions(&[a1, a2, b]).len(), 1);
        let many: Vec<Candidate> = (0..12).map(|i| cand(i, &format!("Acme revenue for 2024 was {} billion dollars.", i + 1), None)).collect();
        assert_eq!(contradictions(&many).len(), MAX_CONTRADICTIONS);
    }

    #[test]
    fn dates_are_found_in_four_forms_and_blanked_out_of_the_sentence() {
        let (found, blanked) = find_dates("It came on 2025-09-14, was updated on 14 September 2025 and again on September 14th, 2025; the plan is for March 2026.");
        assert_eq!(found, vec![Stated::Day(day(2025, 9, 14)), Stated::Day(day(2025, 9, 14)), Stated::Day(day(2025, 9, 14)), Stated::Month(2026, 3)]);
        assert!(!blanked.contains("2025") && !blanked.contains("September") && !blanked.contains("March"), "{blanked}");
        assert!(blanked.contains("It came on") && blanked.contains("the plan is for"), "{blanked}");
        // "may" the verb is not the month, and a day that does not exist is not a day (what is left is the month).
        assert!(find_dates("It may 2025 be late").0.is_empty());
        assert!(!find_dates("on 31 February 2025").0.iter().any(|s| matches!(s, Stated::Day(_))));
        assert!(find_dates("nothing here").0.is_empty());
    }

    #[test]
    fn a_question_is_answered_only_when_a_passage_holds_its_words_and_the_figure_or_date_it_asks_for() {
        let pune = ("Pune has a population of 7.4 million people.", "Pune - Wikipedia");
        assert_eq!(answerability("What is the population of Pune?", &[pune]), Answerability::Answered);
        // Another city: the topic word is there and the place is not.
        assert_eq!(answerability("What is the population of Atlantis?", &[pune]), Answerability::Unanswered("no source I read says it"));
        // About the right thing, but no figure.
        let none = ("Pune is a city in Maharashtra with a large population of students.", "Pune");
        assert_eq!(answerability("What is the population of Pune?", &[none]), Answerability::Unanswered("the sources are about it, but none of them states the figure that was asked for"));
        // A date is asked for.
        assert_eq!(answerability("When was Rust 1.90 released?", &[("Rust 1.90 was released on 14 September 2025.", "Rust blog")]), Answerability::Answered);
        assert_eq!(
            answerability("When was Rust 1.90 released?", &[("Rust 1.90 was released with a faster linker.", "Rust blog")]),
            Answerability::Unanswered("the sources are about it, but none of them states the date that was asked for")
        );
        // The title can hold the subject that the snippet leaves out.
        assert_eq!(answerability("population of Pune", &[("7.4 million people live there.", "Pune population")]), Answerability::Answered);
        // Nothing to look at.
        assert_eq!(answerability("anything", &[]), Answerability::Unanswered("no source I read says anything about it"));
        // The second passage may be the one that answers.
        assert_eq!(answerability("What is the population of Pune?", &[none, pune]), Answerability::Answered);
    }
}
