//! The memory's dream: tidying what the person asked Aqua to remember, by suggestion (card R6-36).
//!
//! Two kinds of untidiness build up in a list of facts that a person adds to over months: the same thing said twice in other words ("my sister is called Asha", "my sister's name
//! is Asha"), and words that were true on one day and say nothing on another ("my dentist appointment is tomorrow"). This module **finds** them; it changes nothing.
//!
//! - **A suggestion is not a fact.** A finding goes into the review queue ([`super::store::Proposal`]), and becomes a change only when the person accepts it in the app (or, for a
//!   correction, says "yes, remember that"). Poisoning is the risk a memory has, so a write has three doors, all the person's, and this is none of them: it only puts a question
//!   at the door. Nothing the model reads comes from the queue, and a suggestion is made from the facts that are there, which are the person's own words.
//! - **No model.** The finding is by rules, so that it can be checked, and it does not read anything but the person's own facts. A private fact (health, money, papers) is left out
//!   of it as it is left out of a prompt.
//! - **What it will not merge:** two facts that differ in a negation or in a number, or that are about different things. "I like coffee" and "I do not like coffee" share every
//!   meaningful word, and are not the same fact.
//! - **Lessons from corrections.** When the person corrects an answer ("no, it's Asha, not Anna"), their own words, and only theirs, are offered as a lesson, a fact of its own kind;
//!   it is written only when they confirm. The assistant's wrong answer is not in it: it may have come from a page.
//!
//! A nightly flow runs [`consolidate`] through the tool `memory.consolidate` (the template `memory_dream`); the app lists the queue.

use super::retrieve::tokens;
use super::store::{Fact, FactStore, WriteError, MAX_CHARS, MAX_FACTS};
use chrono::{DateTime, Datelike, Duration, Local, Months, NaiveDate, TimeZone, Weekday};
use serde::Serialize;
use std::collections::BTreeSet;

/// Two facts are the same when this share of their meaningful words is common (or the shorter is wholly in the longer, and has at least three).
pub const NEAR_SAME_SHARE: f32 = 0.75;
/// The most suggestions one run makes: a person looks at a handful, not at a flood.
pub const MAX_PER_RUN: usize = 20;
/// A correction is offered as a lesson only just after the answer it corrects, and a "yes, remember that" counts only just after that.
pub const CORRECTION_WINDOW_SECS: i64 = 600;

const NEGATIONS: &[&str] = &[
    "not", "no", "never", "none", "nobody", "nothing", "nowhere", "without", "cannot", "cant", "can't", "nor", "neither", "dont", "doesnt", "didnt", "isnt", "arent", "wont", "wasnt", "werent", "hasnt",
    "havent", "hadnt", "shouldnt", "wouldnt", "couldnt",
];

fn words_all(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '\u{2019}')).filter(|w| !w.is_empty()).map(|w| w.replace('\u{2019}', "'")).collect()
}

fn negations(text: &str) -> BTreeSet<String> {
    words_all(text).into_iter().map(|w| w.replace('\'', "")).filter(|w| NEGATIONS.contains(&w.as_str())).collect()
}

fn numbers(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_ascii_digit()).filter(|d| !d.is_empty()).map(str::to_string).collect()
}

/// What a run found, before it is a suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub kind: &'static str,
    pub fact_ids: Vec<i64>,
    pub keep_id: Option<i64>,
    pub new_text: String,
    pub reason: String,
}

fn says_the_same(a: &Fact, b: &Fact) -> bool {
    if a.kind != b.kind || a.sensitive || b.sensitive {
        return false;
    }
    if negations(&a.text) != negations(&b.text) || numbers(&a.text) != numbers(&b.text) {
        return false;
    }
    let ta: BTreeSet<String> = tokens(&a.text).into_iter().collect();
    let tb: BTreeSet<String> = tokens(&b.text).into_iter().collect();
    if ta.len() < 2 || tb.len() < 2 {
        return false;
    }
    let jaccard = ta.intersection(&tb).count() as f32 / ta.union(&tb).count() as f32;
    let (small, big) = if ta.len() <= tb.len() { (&ta, &tb) } else { (&tb, &ta) };
    jaccard >= NEAR_SAME_SHARE || (small.len() >= 3 && small.is_subset(big))
}

/// The facts that say what another already says. Of two, the one that says more stays (of two that say as much, the older), and the other is what a merge would forget. A fact
/// is in at most one finding of a run.
pub fn duplicates(facts: &[Fact]) -> Vec<Found> {
    let mut sorted: Vec<&Fact> = facts.iter().collect();
    sorted.sort_by_key(|f| f.id);
    let mut used: BTreeSet<i64> = BTreeSet::new();
    let mut out = Vec::new();
    for (i, a) in sorted.iter().enumerate() {
        if used.contains(&a.id) {
            continue;
        }
        for b in sorted.iter().skip(i + 1) {
            if used.contains(&b.id) || !says_the_same(a, b) {
                continue;
            }
            let (keep, drop) = if tokens(&b.text).len() > tokens(&a.text).len() { (*b, *a) } else { (*a, *b) };
            out.push(Found {
                kind: "merge",
                fact_ids: vec![drop.id],
                keep_id: Some(keep.id),
                new_text: keep.text.clone(),
                reason: format!("\"{}\" already says what \"{}\" says", keep.text, drop.text),
            });
            used.insert(a.id);
            used.insert(b.id);
            break;
        }
    }
    out
}

const WEEKDAYS: &[(&str, Weekday)] = &[
    ("monday", Weekday::Mon),
    ("tuesday", Weekday::Tue),
    ("wednesday", Weekday::Wed),
    ("thursday", Weekday::Thu),
    ("friday", Weekday::Fri),
    ("saturday", Weekday::Sat),
    ("sunday", Weekday::Sun),
];
/// A word before a date that already carries the preposition ("by tomorrow" is "by Thursday 8 October 2026", not "by on ...").
const PREPOSITIONS: &[&str] = &["by", "on", "until", "till", "before", "after", "from", "since", "for", "during", "at"];
const TIMES_OF_DAY: &[&str] = &["morning", "afternoon", "evening", "night"];
const SMALL_NUMBERS: &[(&str, i64)] = &[
    ("a", 1), ("an", 1), ("one", 1), ("two", 2), ("three", 3), ("four", 4), ("five", 5), ("six", 6), ("seven", 7), ("eight", 8), ("nine", 9), ("ten", 10), ("eleven", 11), ("twelve", 12),
];

/// A word of a text and where it is (byte offsets), words being runs of letters and digits.
fn words_at(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((s, i, text[s..i].to_lowercase()));
        }
    }
    if let Some(s) = start {
        out.push((s, text.len(), text[s..].to_lowercase()));
    }
    out
}

fn day_name(d: NaiveDate) -> String {
    d.format("%A %-d %B %Y").to_string()
}

fn weekday_from(d: NaiveDate, wd: Weekday, strictly_after: bool) -> NaiveDate {
    let mut ahead = (wd.num_days_from_monday() as i64 - d.weekday().num_days_from_monday() as i64).rem_euclid(7);
    if strictly_after && ahead == 0 {
        ahead = 7;
    }
    d + Duration::days(ahead)
}

/// What a phrase is, as a date to be written.
enum Said {
    Day(NaiveDate),
    Night(NaiveDate),
    WeekOf(NaiveDate),
    Month(NaiveDate),
}

/// The phrase that starts at word `i`, how many words it takes, and what it means from the day `d`.
fn phrase_at(words: &[(usize, usize, String)], i: usize, d: NaiveDate) -> Option<(usize, Said)> {
    let w = |k: usize| words.get(i + k).map(|x| x.2.as_str());
    match w(0)? {
        "day" if w(1) == Some("after") && w(2) == Some("tomorrow") => Some((3, Said::Day(d + Duration::days(2)))),
        "tomorrow" => Some((1, Said::Day(d + Duration::days(1)))),
        "yesterday" => Some((1, Said::Day(d - Duration::days(1)))),
        "today" => Some((1, Said::Day(d))),
        "tonight" => Some((1, Said::Night(d))),
        "next" if w(1) == Some("week") => Some((2, Said::WeekOf(d + Duration::days(7 - d.weekday().num_days_from_monday() as i64)))),
        "next" if w(1) == Some("month") => Some((2, Said::Month(d.checked_add_months(Months::new(1))?))),
        which @ ("next" | "this") => {
            let wd = WEEKDAYS.iter().find(|(n, _)| Some(*n) == w(1))?.1;
            Some((2, Said::Day(weekday_from(d, wd, which == "next"))))
        }
        "in" => {
            let n = w(1).and_then(|x| x.parse::<i64>().ok().or_else(|| SMALL_NUMBERS.iter().find(|(n, _)| *n == x).map(|(_, v)| *v)))?;
            if !(1..=366).contains(&n) {
                return None;
            }
            match w(2)? {
                "day" | "days" => Some((3, Said::Day(d + Duration::days(n)))),
                "week" | "weeks" => Some((3, Said::Day(d + Duration::days(7 * n)))),
                "month" | "months" => Some((3, Said::Month(d.checked_add_months(Months::new(n as u32))?))),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The text with its relative dates written as the dates they were on the day it was said (`said_at`), and what was replaced by what. `None` when it has none. "Next Friday" is read
/// as the first Friday after the day it was said, and "this Friday" as the coming one (the same day, if it was said on a Friday); that is a reading, which is why a change is a
/// suggestion and not an edit.
pub fn absolute_dates_in(text: &str, said_at: DateTime<Local>) -> Option<(String, Vec<(String, String)>)> {
    let d = said_at.date_naive();
    let words = words_at(text);
    let mut edits: Vec<(usize, usize, String, String)> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let Some((len, said)) = phrase_at(&words, i, d) else {
            i += 1;
            continue;
        };
        // "The day after tomorrow" is replaced whole, its "the" too.
        let first_word = if matches!(said, Said::Day(_)) && words[i].2 == "day" && i > 0 && words[i - 1].2 == "the" { i - 1 } else { i };
        let after_preposition = first_word > 0 && PREPOSITIONS.contains(&words[first_word - 1].2.as_str());
        let mut end_word = i + len - 1;
        let time_of_day = match (&said, words.get(i + len).map(|x| x.2.as_str())) {
            (Said::Day(_), Some(t)) if TIMES_OF_DAY.contains(&t) => {
                end_word += 1;
                Some(t.to_string())
            }
            _ => None,
        };
        let lead = |preposition: &str| if after_preposition { String::new() } else { format!("{preposition} ") };
        let written = match (&said, &time_of_day) {
            (Said::Day(date), Some(t)) => format!("{}the {t} of {}", lead("on"), day_name(*date)),
            (Said::Day(date), None) => format!("{}{}", lead("on"), day_name(*date)),
            (Said::Night(date), _) => format!("{}the night of {}", lead("on"), day_name(*date)),
            (Said::WeekOf(date), _) => format!("{}the week of {}", lead("in"), day_name(*date)),
            (Said::Month(date), _) => format!("{}{}", lead("in"), date.format("%B %Y")),
        };
        let (from, to) = (words[first_word].0, words[end_word].1);
        edits.push((from, to, text[from..to].to_string(), written));
        i = end_word + 1;
    }
    if edits.is_empty() {
        return None;
    }
    let mut out = text.to_string();
    for (from, to, _, written) in edits.iter().rev() {
        out.replace_range(*from..*to, written);
    }
    if out.chars().count() > MAX_CHARS {
        return None;
    }
    Some((out, edits.into_iter().map(|(_, _, was, now)| (was, now)).collect()))
}

/// The facts with a relative date in them, as they would read with the real one. Only a fact that has a day it was written on, and that is not private.
pub fn dates(facts: &[Fact]) -> Vec<Found> {
    let mut out = Vec::new();
    for f in facts.iter().filter(|f| !f.sensitive && f.created > 0) {
        let Some(said_at) = Local.timestamp_opt(f.created, 0).single() else { continue };
        let Some((new_text, edits)) = absolute_dates_in(&f.text, said_at) else { continue };
        if new_text == f.text {
            continue;
        }
        let was = edits.iter().map(|(a, _)| format!("\"{a}\"")).collect::<Vec<_>>().join(", ");
        out.push(Found { kind: "dates", fact_ids: vec![f.id], keep_id: None, new_text, reason: format!("it says {was}, and it was written on {}", said_at.format("%A %-d %B %Y")) });
    }
    out
}

/// What one run did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub looked_at: usize,
    pub merges: usize,
    pub dates: usize,
    /// Findings that had been made before (waiting, or decided: a "no" is not asked again).
    pub known: usize,
    /// The queue was full: the run stopped, and the rest is for the next one.
    pub queue_full: bool,
}

impl Report {
    /// One sentence, with no fact in it (the tool says it, and a flow shows it).
    pub fn summary(&self) -> String {
        let new = self.merges + self.dates;
        let mut s = if new == 0 { format!("Nothing new to suggest (looked at {} facts).", self.looked_at) } else { format!("{new} new suggestion{} to look at: {} merge{}, {} date{}.", if new == 1 { "" } else { "s" }, self.merges, if self.merges == 1 { "" } else { "s" }, self.dates, if self.dates == 1 { "" } else { "s" }) };
        if self.known > 0 {
            s.push_str(&format!(" {} already made before.", self.known));
        }
        if self.queue_full {
            s.push_str(" The list is full: look at it first.");
        }
        s
    }
}

/// Looks through the facts and puts what it finds in the review queue. Changes no fact.
pub fn consolidate(store: &FactStore, now: i64) -> Report {
    let facts = store.list(false, MAX_FACTS as usize);
    let mut report = Report { looked_at: facts.len(), ..Report::default() };
    if store.incognito() {
        return report;
    }
    let mut found = duplicates(&facts);
    found.extend(dates(&facts));
    found.truncate(MAX_PER_RUN);
    for f in found {
        match store.add_proposal(f.kind, &f.fact_ids, f.keep_id, &f.new_text, &f.reason, now) {
            Ok(Some(_)) if f.kind == "merge" => report.merges += 1,
            Ok(Some(_)) => report.dates += 1,
            Ok(None) => report.known += 1,
            Err(WriteError::QueueFull(_)) => {
                report.queue_full = true;
                break;
            }
            Err(_) => {}
        }
    }
    report
}

const CORRECTION_BASES: &[&str] = &[
    "no", "nope", "wrong", "incorrect", "not quite", "that's wrong", "that is wrong", "that's not right", "that is not right", "that's incorrect", "that is incorrect", "that's not correct",
    "that is not correct", "you're wrong", "you are wrong", "you got that wrong", "you got it wrong",
];
/// Verbs that start a command: "no, open Chrome instead" corrects a request, and is not a fact about the person or the world.
const COMMANDS: &[&str] = &[
    "open", "play", "search", "send", "call", "set", "turn", "go", "show", "tell", "find", "make", "put", "take", "close", "start", "stop", "use", "check", "write", "read", "add", "remove", "delete",
    "click", "type", "run", "give", "bring", "get", "look", "try", "do", "pause", "resume", "skip", "mute", "unmute", "increase", "decrease", "change", "switch",
];
/// A correction states something: one of these is in it ("the 5th of November" says nothing by itself).
const CLAIM_WORDS: &[&str] = &[
    "is", "are", "was", "were", "it's", "that's", "he's", "she's", "they're", "means", "called", "named", "stands", "not", "isn't", "wasn't", "am", "i'm", "live", "lives", "born", "work", "works",
    "prefer", "like", "likes", "has", "have", "had", "calls", "spelled", "spelt", "pronounced",
];

fn capitalised(text: &str) -> String {
    let mut c = text.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// What the person's correction teaches, in their own words and nothing else: `Correction: <what they said after "no,">`. `None` when the sentence is not a correction (a
/// question, a command, a bare "that's wrong" with nothing said after it, something with no claim in it).
pub fn lesson_from_correction(said: &str) -> Option<String> {
    if said.trim().ends_with('?') {
        return None;
    }
    let t = super::plain(said);
    let mut markers: Vec<String> = Vec::new();
    for base in CORRECTION_BASES {
        for sep in [", ", ". ", ": ", " - "] {
            markers.push(format!("{base}{sep}"));
        }
    }
    for open in ["actually, ", "actually ", "i meant ", "i said ", "what i said was ", "what i meant was "] {
        markers.push(open.to_string());
    }
    let markers: Vec<&str> = markers.iter().map(String::as_str).collect();
    let content = super::after_any(&t, &markers)?;
    let content = content.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | '-' | ' ')).to_string();
    let words = words_all(&content);
    if !(3..=30).contains(&words.len()) {
        return None;
    }
    if words.first().is_some_and(|w| COMMANDS.contains(&w.as_str())) {
        return None;
    }
    if !words.iter().any(|w| CLAIM_WORDS.contains(&w.as_str())) {
        return None;
    }
    let text = format!("Correction: {}.", capitalised(content.trim_end_matches(['.', '!'])));
    (text.chars().count() <= MAX_CHARS).then_some(text)
}

/// A correction the person just made, offered as a lesson for them to confirm. `true` when a suggestion was made (not for one that was made before, or when the store is off).
pub fn note_correction_in(store: &FactStore, said: &str, now: i64) -> bool {
    let Some(text) = lesson_from_correction(said) else { return false };
    matches!(store.add_proposal("lesson", &[], None, &text, "you corrected my last answer, and this is what you said", now), Ok(Some(_)))
}

/// [`note_correction_in`] on the store of this program (when the memory is on).
pub fn note_correction(said: &str) -> bool {
    if !super::enabled() {
        return false;
    }
    let Some(store) = super::store() else { return false };
    note_correction_in(&store, said, super::now())
}

/// "Yes, remember that" (`accept`) or "don't remember that": the newest correction that is waiting and not older than [`CORRECTION_WINDOW_SECS`].
pub fn answer_to_lesson_in(store: &FactStore, accept: bool, now: i64) -> String {
    let waiting = store.proposals(Some("pending"), 50).into_iter().find(|p| p.kind == "lesson" && now - p.created <= CORRECTION_WINDOW_SECS);
    let Some(p) = waiting else { return "I don't have a correction waiting to be remembered.".to_string() };
    match store.decide_proposal(p.id, accept, "user", now) {
        Ok(done) if accept => format!("Okay, I'll remember that: {}", done.new_text.trim_end_matches('.')),
        Ok(_) => "Okay, I won't remember it.".to_string(),
        Err(e) => format!("I couldn't do that: {e}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wednesday 7 October 2026, ten in the morning, the day all of these are said.
    fn wednesday() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 7, 10, 0, 0).single().expect("a day that exists")
    }

    fn store(name: &str) -> FactStore {
        let dir = std::env::temp_dir().join(format!("aqua_user_facts_dream_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        FactStore::open(dir.join("user_memory.db")).unwrap()
    }

    fn fact(id: i64, text: &str) -> Fact {
        Fact { id, kind: "fact".into(), text: text.into(), source: "user".into(), created: wednesday().timestamp(), last_used: 0, uses: 0, sensitive: false }
    }

    fn rewrite(text: &str) -> Option<String> {
        absolute_dates_in(text, wednesday()).map(|(t, _)| t)
    }

    #[test]
    fn the_same_thing_said_twice_is_found_and_what_differs_in_a_negation_a_number_or_a_name_is_not_the_same() {
        let found = duplicates(&[fact(1, "My sister's name is Asha"), fact(2, "My sister's name is Asha and she lives in Pune"), fact(3, "I live in Pune")]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].fact_ids.clone(), found[0].keep_id, found[0].kind), (vec![1], Some(2), "merge"), "the one that says more stays");
        assert!(found[0].reason.contains("already says what"), "{}", found[0].reason);
        // Equal in every meaningful word, and not the same: a negation, a number, another person.
        for (a, b) in [
            ("I like coffee in the morning", "I do not like coffee in the morning"),
            ("I never drink coffee after lunch", "I drink coffee after lunch"),
            ("My sister is 30 years old today", "My sister is 31 years old today"),
            ("My sister's name is Asha", "My sister's name is Anika"),
            ("My sister lives in Pune", "My brother lives in Pune"),
        ] {
            assert!(duplicates(&[fact(1, a), fact(2, b)]).is_empty(), "{a} / {b}");
        }
        // A private fact is never put beside another, and a preference is not a fact.
        let mut private = fact(2, "My salary is paid on the first of the month");
        private.sensitive = true;
        assert!(duplicates(&[fact(1, "My salary is paid on the first of the month"), private]).is_empty());
        let mut pref = fact(2, "I prefer short answers please");
        pref.kind = "preference".into();
        assert!(duplicates(&[fact(1, "I prefer short answers please"), pref]).is_empty(), "different kinds");
        // Three that say one thing are one finding for the pair first met, and a fact is in one finding only.
        let three = duplicates(&[fact(1, "My sister's name is Asha"), fact(2, "My sister's name is Asha"), fact(3, "My sister's name is Asha")]);
        assert_eq!(three.len(), 1, "{three:?}");
    }

    #[test]
    fn a_relative_date_is_written_as_the_date_it_was_on_the_day_it_was_said() {
        // Said on Wednesday 7 October 2026.
        for (said, written) in [
            ("My dentist appointment is tomorrow", "My dentist appointment is on Thursday 8 October 2026"),
            ("I have to pay the bill by tomorrow", "I have to pay the bill by Thursday 8 October 2026"),
            ("The party is the day after tomorrow", "The party is on Friday 9 October 2026"),
            ("Pay it by the day after tomorrow", "Pay it by Friday 9 October 2026"),
            ("I went to the clinic yesterday", "I went to the clinic on Tuesday 6 October 2026"),
            ("I have a meeting today", "I have a meeting on Wednesday 7 October 2026"),
            ("I fly home tonight", "I fly home on the night of Wednesday 7 October 2026"),
            ("Call the bank tomorrow morning", "Call the bank on the morning of Thursday 8 October 2026"),
            ("We meet next Friday", "We meet on Friday 9 October 2026"),
            ("We meet this Friday", "We meet on Friday 9 October 2026"),
            ("The review is this Wednesday", "The review is on Wednesday 7 October 2026"),
            ("The review is next Wednesday", "The review is on Wednesday 14 October 2026"),
            ("The exam is in 3 days", "The exam is on Saturday 10 October 2026"),
            ("The exam is in two weeks", "The exam is on Wednesday 21 October 2026"),
            ("The lease ends in 2 months", "The lease ends in December 2026"),
            ("The trip is next week", "The trip is in the week of Monday 12 October 2026"),
            ("The trip is next month", "The trip is in November 2026"),
            ("I leave tomorrow and come back next Monday", "I leave on Thursday 8 October 2026 and come back on Monday 12 October 2026"),
        ] {
            assert_eq!(rewrite(said).as_deref(), Some(written), "{said}");
        }
        for timeless in ["My sister's name is Asha", "I prefer short answers", "The meeting is on Friday 9 October 2026", "I was born in 1990", "I will be there at 5", "Tomorrowland is a festival"] {
            assert_eq!(rewrite(timeless), None, "{timeless}");
        }
        // Said once, written once: the result has no relative date left in it.
        let once = rewrite("Pay it tomorrow or next week").unwrap();
        assert_eq!(rewrite(&once), None, "{once}");
        // Said at the end of a month and a year.
        let new_year = Local.with_ymd_and_hms(2026, 12, 31, 23, 0, 0).single().unwrap();
        assert_eq!(absolute_dates_in("It opens tomorrow", new_year).unwrap().0, "It opens on Friday 1 January 2027");
        assert_eq!(absolute_dates_in("It opens next month", new_year).unwrap().0, "It opens in January 2027");
    }

    #[test]
    fn a_run_puts_findings_in_the_queue_changes_no_fact_and_does_not_make_them_twice() {
        let s = store("run");
        let when = wednesday().timestamp();
        s.remember("My sister's name is Asha", "user", when).unwrap();
        s.remember("My sister's name is Asha and she lives in Pune", "user", when).unwrap();
        s.remember("My dentist appointment is tomorrow", "user", when).unwrap();
        s.remember("My salary is paid tomorrow", "user", when).unwrap();
        let before: Vec<String> = s.list(true, 20).into_iter().map(|f| f.text).collect();
        let report = consolidate(&s, when);
        assert_eq!((report.merges, report.dates, report.known, report.queue_full), (1, 1, 0, false), "{report:?}");
        assert_eq!(s.list(true, 20).into_iter().map(|f| f.text).collect::<Vec<_>>(), before, "no fact was changed by looking");
        let waiting = s.proposals(Some("pending"), 10);
        assert_eq!(waiting.len(), 2, "the private fact (a salary) is not read: {waiting:?}");
        assert!(!waiting.iter().any(|p| p.new_text.contains("salary")));
        assert!(report.summary().contains("2 new suggestions") && !report.summary().contains("Asha"), "{}", report.summary());
        // Again: nothing new, and both are known.
        let again = consolidate(&s, when + 60);
        assert_eq!((again.merges, again.dates, again.known), (0, 0, 2), "{again:?}");
        assert!(again.summary().contains("2 already made before"));
    }

    #[test]
    fn accepting_a_merge_forgets_the_shorter_a_date_rewrites_the_fact_in_place_and_a_no_is_not_asked_again() {
        let s = store("decide");
        let when = wednesday().timestamp();
        let short = s.remember("My sister's name is Asha", "user", when).unwrap();
        let long = s.remember("My sister's name is Asha and she lives in Pune", "user", when).unwrap();
        let dentist = s.remember("My dentist appointment is tomorrow", "user", when).unwrap();
        consolidate(&s, when);
        let waiting = s.proposals(Some("pending"), 10);
        let merge = waiting.iter().find(|p| p.kind == "merge").unwrap();
        let date = waiting.iter().find(|p| p.kind == "dates").unwrap();
        assert_eq!((merge.fact_ids.clone(), merge.keep_id), (vec![short.id], Some(long.id)));
        let done = s.decide_proposal(merge.id, true, "ui", when + 1).unwrap();
        assert_eq!(done.status, "accepted");
        assert!(s.get(short.id).is_none() && s.get(long.id).is_some(), "the one that says less is gone");
        let done = s.decide_proposal(date.id, true, "ui", when + 2).unwrap();
        assert_eq!(done.status, "accepted");
        let rewritten = s.get(dentist.id).expect("the same fact");
        assert_eq!((rewritten.text.as_str(), rewritten.created), ("My dentist appointment is on Thursday 8 October 2026", when), "the same id and the day it was written");
        assert!(s.decide_proposal(date.id, true, "ui", when + 3).is_err(), "decided once");
        // A "no" stays, and the same finding is not made again.
        let other = store("decide_no");
        other.remember("The meeting is tomorrow", "user", when).unwrap();
        consolidate(&other, when);
        let p = other.proposals(Some("pending"), 5).remove(0);
        assert_eq!(other.decide_proposal(p.id, false, "ui", when + 1).unwrap().status, "rejected");
        let again = consolidate(&other, when + 10);
        assert_eq!((again.dates, again.known), (0, 1), "{again:?}");
        assert_eq!(other.get(1).unwrap().text, "The meeting is tomorrow", "a no changes nothing");
    }

    #[test]
    fn a_suggestion_about_facts_that_are_gone_is_dropped_and_forgetting_a_fact_forgets_what_was_suggested_about_it() {
        let s = store("forget");
        let when = wednesday().timestamp();
        let a = s.remember("My sister's name is Asha", "user", when).unwrap();
        let b = s.remember("My sister's name is Asha and she lives in Pune", "user", when).unwrap();
        s.remember("My dentist appointment is tomorrow", "user", when).unwrap();
        consolidate(&s, when);
        assert_eq!(s.proposals(None, 10).len(), 2);
        // Forget the fact a merge would forget: the suggestion about it goes too, with the words it held.
        assert!(s.forget(a.id));
        assert!(s.proposals(None, 10).iter().all(|p| p.kind != "merge"), "{:?}", s.proposals(None, 10));
        // Forget the fact a date change was about.
        let dentist = s.list(false, 10).into_iter().find(|f| f.text.contains("dentist")).unwrap();
        s.forget(dentist.id);
        assert!(s.proposals(None, 10).is_empty());
        // A merge's kept fact forgotten: the words of the suggestion are the fact's own words, so it goes with it.
        s.add_proposal("merge", &[b.id], Some(b.id), "My sister's name is Asha and she lives in Pune", "x", when).unwrap();
        assert!(s.forget(b.id));
        assert!(s.proposals(None, 10).is_empty(), "nothing is left that holds a forgotten fact's words");
        // Clearing the memory clears the queue.
        s.remember("The meeting is tomorrow", "user", when).unwrap();
        consolidate(&s, when);
        assert_eq!(s.proposals(None, 10).len(), 1);
        s.clear();
        assert!(s.proposals(None, 10).is_empty());
    }

    #[test]
    fn the_queue_has_a_limit_secrets_and_incognito_are_refused_and_a_suggestion_that_went_stale_is_dropped() {
        let s = store("limits");
        let when = wednesday().timestamp();
        for i in 0..50 {
            s.add_proposal("lesson", &[], None, &format!("Correction: the number is {i}."), "x", when).unwrap();
        }
        assert_eq!(s.add_proposal("lesson", &[], None, "Correction: one more thing is true.", "x", when), Err(WriteError::QueueFull(50)));
        assert!(WriteError::QueueFull(50).to_string().contains("50 suggestions"));
        s.remember("The meeting is tomorrow", "user", when).unwrap();
        assert!(consolidate(&s, when).queue_full, "a run stops when the list is full");
        assert_eq!(s.add_proposal("lesson", &[], None, "Correction: the key is password=hunter2secret99.", "x", when), Err(WriteError::LooksLikeASecret));
        // A suggestion about a fact that is not there (it was forgotten some other way, or never was) is dropped, not done, and is a "no" from then on.
        let t = store("stale");
        let stale = t.add_proposal("dates", &[999], None, "The meeting is on Thursday 8 October 2026", "x", when).unwrap().unwrap();
        let err = t.decide_proposal(stale.id, true, "ui", when + 1).unwrap_err();
        assert!(err.contains("changed since"), "{err}");
        assert_eq!(t.proposal(stale.id).unwrap().status, "rejected");
        assert_eq!(t.count(), 0, "nothing was written");
        // Incognito: nothing is looked at or queued.
        let u = store("incognito");
        u.remember("The meeting is tomorrow", "user", when).unwrap();
        u.set_incognito(true).unwrap();
        let r = consolidate(&u, when);
        assert_eq!((r.merges, r.dates), (0, 0));
        assert_eq!(u.add_proposal("lesson", &[], None, "Correction: the meeting is on Friday.", "x", when), Err(WriteError::Incognito));
    }

    #[test]
    fn a_correction_is_the_persons_own_words_after_the_no_and_a_question_a_command_or_a_bare_no_is_not_one() {
        for (said, lesson) in [
            ("No, my sister's name is Asha", "Correction: My sister's name is Asha."),
            ("Actually it's Asha, not Anna", "Correction: It's Asha, not Anna."),
            ("That's wrong. The meeting is on Friday", "Correction: The meeting is on Friday."),
            ("Nope, I live in Pune now", "Correction: I live in Pune now."),
            ("no: the capital of Australia is Canberra", "Correction: The capital of Australia is Canberra."),
        ] {
            assert_eq!(lesson_from_correction(said).as_deref(), Some(lesson), "{said}");
        }
        for not_a_correction in [
            "No",
            "No, thanks",
            "That's wrong",
            "No, what about Friday?",
            "No, open Chrome instead",
            "No, play something else",
            "Actually, can you check the weather",
            "no one called me",
            "Nothing is wrong with it",
            "yes, that's right",
            "No, ok",
            "I meant the 5th of November",
        ] {
            assert_eq!(lesson_from_correction(not_a_correction), None, "{not_a_correction}");
        }
        // Only the person's words: nothing of the answer that was corrected is in a lesson (the function is not given it). Whether the store keeps what they said is the store's rule:
        // it refuses what looks like a secret, and the suggestion is then not made.
        let s = store("secret_lesson");
        assert!(lesson_from_correction("No, the key is password=hunter2secret99").is_some());
        assert!(!note_correction_in(&s, "No, the key is password=hunter2secret99", 1_000), "a secret is not offered to be remembered");
        assert!(s.proposals(None, 5).is_empty());
    }

    #[test]
    fn a_lesson_waits_for_a_yes_and_a_yes_keeps_it_as_a_fact_of_its_own_kind_and_a_no_drops_it() {
        let s = store("lesson");
        let now = super::super::now();
        assert!(note_correction_in(&s, "No, my sister's name is Asha", now));
        assert!(!note_correction_in(&s, "No, my sister's name is Asha", now + 5), "the same lesson is not offered twice");
        assert!(!note_correction_in(&s, "No, what about Friday?", now));
        assert_eq!(s.count(), 0, "a correction is not a fact until it is confirmed");
        let waiting = s.proposals(Some("pending"), 5);
        assert_eq!((waiting.len(), waiting[0].kind.as_str()), (1, "lesson"));
        // Through the person's own sentence.
        let said_yes = super::super::handle_utterance_in(&s, "Yes, remember that").expect("a request to keep a correction");
        assert!(said_yes.starts_with("Okay, I'll remember that: Correction: My sister's name is Asha"), "{said_yes}");
        let kept = s.list(false, 5);
        assert_eq!((kept.len(), kept[0].kind.as_str(), kept[0].text.as_str()), (1, "lesson", "Correction: My sister's name is Asha."));
        assert_eq!(super::super::handle_utterance_in(&s, "yes remember that").unwrap(), "I don't have a correction waiting to be remembered.");
        // A no.
        let t = store("lesson_no");
        assert!(note_correction_in(&t, "Actually it's Asha, not Anna", now));
        assert_eq!(super::super::handle_utterance_in(&t, "Don't remember that").unwrap(), "Okay, I won't remember it.");
        assert_eq!(t.count(), 0);
        assert_eq!(answer_to_lesson_in(&t, true, now + 1), "I don't have a correction waiting to be remembered.");
        // Too late: a lesson older than ten minutes is not what "yes" means now.
        let u = store("lesson_late");
        assert!(note_correction_in(&u, "No, the meeting is on Friday", now));
        assert_eq!(answer_to_lesson_in(&u, true, now + CORRECTION_WINDOW_SECS + 1), "I don't have a correction waiting to be remembered.");
        assert_eq!(u.count(), 0);
        // "Forget that correction" is a no to the lesson waiting, not a search for facts that hold the word.
        let v = store("lesson_forget");
        v.remember("The correction fluid is in the top drawer", "user", now).unwrap();
        assert!(note_correction_in(&v, "No, the meeting is on Friday", now));
        assert_eq!(super::super::handle_utterance_in(&v, "forget that correction").unwrap(), "Okay, I won't remember it.");
        assert_eq!(v.count(), 1, "the fact that has the word in it was not forgotten");
    }
}
