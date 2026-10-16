//! What people say about a time, written as the digits the parsers of times read (`flows::schedule_text`, `reminders`).
//!
//! "quarter past five" is `5:15`, "twenty-five minutes" is `25 minutes`, "five p.m." is `5 pm`, "tonight at eight" is `today at 8 pm` and "tomorrow morning" is
//! `tomorrow at 9 am`. Only what is plainly about a time is rewritten: a number word that is not beside a time word ("buy three apples") is left as it is, and so
//! is every other word of the sentence (a reminder's message is cut out of what is left). Reading the result again changes nothing.

use regex::Regex;
use std::sync::OnceLock;

const SMALL: [&str; 20] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen",
];
const TENS: [(&str, u32); 8] = [("twenty", 20), ("thirty", 30), ("forty", 40), ("fifty", 50), ("sixty", 60), ("seventy", 70), ("eighty", 80), ("ninety", 90)];

/// Words that, before a number, say it is a time or a length of time.
const BEFORE: [&str; 11] = ["at", "by", "around", "before", "until", "till", "in", "for", "after", "within", "about"];
/// Words that, after a number, say it is a time or a length of time.
const AFTER: [&str; 16] = ["am", "pm", "hour", "hours", "hr", "hrs", "minute", "minutes", "min", "mins", "second", "seconds", "sec", "secs", "day", "days"];

/// The words of a token as a lower-case string without the punctuation around it.
fn bare(token: &str) -> String {
    token.trim_matches(|c: char| !c.is_alphanumeric() && c != ':').to_ascii_lowercase()
}

/// The punctuation that follows a token ("five," keeps its comma).
fn trailing(token: &str) -> &str {
    let end = token.trim_end_matches(|c: char| !c.is_alphanumeric()).len();
    &token[end..]
}

fn small(word: &str) -> Option<u32> {
    SMALL.iter().position(|w| *w == word).map(|i| i as u32)
}

/// A number from one to ninety-nine written as one token ("seven", "twenty", "twenty-five") or as digits.
fn word_value(token: &str) -> Option<u32> {
    let w = bare(token);
    if let Ok(n) = w.parse::<u32>() {
        return Some(n);
    }
    if let Some(n) = small(&w) {
        return Some(n);
    }
    if let Some((tens, units)) = w.split_once('-') {
        let t = TENS.iter().find(|(name, _)| *name == tens)?.1;
        return small(units).filter(|u| (1..=9).contains(u)).map(|u| t + u);
    }
    TENS.iter().find(|(name, _)| *name == w).map(|(_, n)| *n)
}

/// A number in words at the start of `tokens`, and how many tokens it takes: "twenty five" (two), "seven" (one), "25" (one).
fn number_at(tokens: &[&str]) -> Option<(u32, usize)> {
    let first = word_value(tokens.first()?)?;
    if TENS.iter().any(|(name, _)| *name == bare(tokens[0])) {
        if let Some(units) = tokens.get(1).and_then(|t| small(&bare(t))).filter(|u| (1..=9).contains(u)) {
            return Some((first + units, 2));
        }
    }
    Some((first, 1))
}

fn is_word_number(token: &str) -> bool {
    let w = bare(token);
    small(&w).is_some() || TENS.iter().any(|(name, _)| *name == w) || w.split_once('-').is_some_and(|(a, b)| TENS.iter().any(|(name, _)| *name == a) && small(b).is_some())
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("a valid pattern"))
}

/// "a.m." and "p.m." as `am` and `pm`; "five o'clock" as "five".
fn abbreviations(text: &str) -> String {
    static AMPM: OnceLock<Regex> = OnceLock::new();
    static OCLOCK: OnceLock<Regex> = OnceLock::new();
    let t = re(&AMPM, r"(?i)\b([ap])\.\s?m\b\.?").replace_all(text, |c: &regex::Captures| format!("{}m", c[1].to_ascii_lowercase())).into_owned();
    re(&OCLOCK, r"(?i)\s+o['’]?clock\b").replace_all(&t, "").into_owned()
}

/// The hour and minute that "half past five", "quarter to six" and "ten past four" mean, as the token that replaces them, and how many tokens they take.
fn clock_phrase(raw: &[&str], i: usize) -> Option<(String, usize)> {
    let word = |k: usize| raw.get(i + k).map(|t| bare(t));
    let first = word(0)?;
    let hour = |k: usize| word_value(raw.get(i + k)?).filter(|h| (1..=12).contains(h));
    let last = |used: usize| trailing(raw[i + used - 1]).to_string();
    let (minutes, to) = match (first.as_str(), word(1).as_deref()) {
        ("half", Some("past")) => (30, false),
        ("quarter", Some("past")) => (15, false),
        ("quarter", Some("to")) => (15, true),
        (_, Some(link @ ("past" | "to"))) => {
            // "ten past four", "twenty to six": a number of minutes, and only where a time is plainly meant.
            let before = i.checked_sub(1).map(|p| bare(raw[p]));
            if !before.is_some_and(|b| matches!(b.as_str(), "at" | "by" | "around" | "before" | "until")) {
                return None;
            }
            let m = word_value(raw[i]).filter(|m| (1..=59).contains(m))?;
            (m, link == "to")
        }
        _ => return None,
    };
    let h = hour(2)?;
    let (h, m) = if to { (if h == 1 { 12 } else { h - 1 }, 60 - minutes) } else { (h, minutes) };
    Some((format!("{h}:{m:02}{}", last(3)), 3))
}

/// Number words that are plainly about a time, as digits; "half past five" as `5:30`.
pub fn digits(text: &str) -> String {
    let text = abbreviations(text);
    let raw: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if let Some((token, used)) = clock_phrase(&raw, i) {
            out.push(token);
            i += used;
            continue;
        }
        if is_word_number(raw[i]) {
            let (n, used) = number_at(&raw[i..]).unwrap_or((0, 1));
            let before = i.checked_sub(1).map(|p| bare(raw[p]));
            let after = raw.get(i + used).map(|t| bare(t));
            let after_before = before.as_deref().is_some_and(|b| BEFORE.contains(&b));
            let in_time = after_before || after.as_deref().is_some_and(|a| AFTER.contains(&a));
            if in_time {
                // "at seven thirty": an hour and then the minutes, when no unit of time follows them (that would be "at 7, 30 minutes").
                if after_before && (1..=12).contains(&n) {
                    if let Some((m, more)) = number_at(&raw[i + used..]).filter(|(m, _)| (10..=59).contains(m)) {
                        let next = raw.get(i + used + more).map(|t| bare(t));
                        if !next.as_deref().is_some_and(|a| AFTER.contains(&a) && a != "am" && a != "pm") {
                            out.push(format!("{n}:{m:02}{}", trailing(raw[i + used + more - 1])));
                            i += used + more;
                            continue;
                        }
                    }
                }
                out.push(format!("{n}{}", trailing(raw[i + used - 1])));
                i += used;
                continue;
            }
        }
        out.push(raw[i].to_string());
        i += 1;
    }
    out.join(" ")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Morning,
    Afternoon,
    Evening,
    Night,
}

impl Part {
    fn of(word: &str) -> Option<Part> {
        match word.to_ascii_lowercase().as_str() {
            "morning" => Some(Part::Morning),
            "afternoon" => Some(Part::Afternoon),
            "evening" => Some(Part::Evening),
            "night" | "tonight" => Some(Part::Night),
            _ => None,
        }
    }

    /// The time a part of the day means when no hour is said.
    fn default_time(self) -> &'static str {
        match self {
            Part::Morning => "9 am",
            Part::Afternoon => "3 pm",
            Part::Evening => "6 pm",
            Part::Night => "8 pm",
        }
    }

    /// The suffix a bare hour takes in this part of the day: "at 7 in the evening" is 7 pm, "at 1 at night" is 1 am.
    fn suffix(self, hour: u32) -> &'static str {
        match self {
            Part::Morning => "am",
            Part::Afternoon | Part::Evening => "pm",
            Part::Night if (6..=11).contains(&hour) => "pm",
            Part::Night => "am",
        }
    }
}

/// "tonight", "this evening", "tomorrow morning", "at 7 in the evening": the day word of the sentence is left, the part of the day becomes a time the parsers read.
pub fn parts_of_day(text: &str) -> String {
    static TONIGHT: OnceLock<Regex> = OnceLock::new();
    static DAY_PART: OnceLock<Regex> = OnceLock::new();
    static IN_PART: OnceLock<Regex> = OnceLock::new();
    static CLOCK: OnceLock<Regex> = OnceLock::new();
    let tonight = re(&TONIGHT, r"(?i)\btonight\b");
    let day_part = re(&DAY_PART, r"(?i)\b(this|tomorrow|(?:on\s+|next\s+)?(?:monday|tuesday|wednesday|thursday|friday|saturday|sunday))\s+(morning|afternoon|evening|night)\b");
    let in_part = re(&IN_PART, r"(?i)\b(?:in\s+the|at)\s+(morning|afternoon|evening|night)\b");
    let mut out = text.to_string();
    let mut part: Option<Part> = None;
    if tonight.is_match(&out) {
        part = Some(Part::Night);
        out = tonight.replace_all(&out, "today").into_owned();
    }
    if let Some(c) = day_part.captures(&out) {
        part = part.or(Part::of(&c[2]));
        let day = if c[1].eq_ignore_ascii_case("this") { "today".to_string() } else { c[1].to_string() };
        out = day_part.replace(&out, day.as_str()).into_owned();
    }
    if let Some(c) = in_part.captures(&out) {
        part = part.or(Part::of(&c[1]));
        out = in_part.replace(&out, "").into_owned();
    }
    let Some(part) = part else { return text.to_string() };
    // An hour said without am or pm takes the part of the day; with none said, the part of the day gives one.
    let clock = re(&CLOCK, r"(?i)\b(at\s+)?(\d{1,2})(?::(\d{2}))?(?:\s*(am|pm)\b)?");
    for c in clock.captures_iter(&out.clone()) {
        let hour: u32 = c[2].parse().unwrap_or(99);
        let is_a_time = c.get(1).is_some() || c.get(3).is_some() || c.get(4).is_some();
        if !is_a_time || !(1..=12).contains(&hour) {
            continue; // a number that is not a time of day ("5 apples")
        }
        if c.get(4).is_some() {
            return normalise_spaces(&out); // the hour says am or pm already
        }
        let whole = c.get(0).map_or("", |m| m.as_str()).to_string();
        let minutes = c.get(3).map_or(String::new(), |m| format!(":{}", m.as_str()));
        out = out.replacen(&whole, &format!("at {hour}{minutes} {}", part.suffix(hour)), 1);
        return normalise_spaces(&out);
    }
    normalise_spaces(&format!("{out} at {}", part.default_time()))
}

fn normalise_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A sentence with its spoken times in the form the parsers read: `abbreviations`, number words and the parts of the day. Doing it twice changes nothing.
pub fn normalise(text: &str) -> String {
    parts_of_day(&digits(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_words_beside_a_time_word_become_digits_and_the_others_stay() {
        for (said, read) in [
            ("remind me at five to call Anna", "remind me at 5 to call Anna"),
            ("in twenty five minutes", "in 25 minutes"),
            ("in twenty-five minutes", "in 25 minutes"),
            ("for ten minutes", "for 10 minutes"),
            ("in two hours and thirty minutes", "in 2 hours and 30 minutes"),
            ("at seven thirty pm", "at 7:30 pm"),
            ("at five twenty five", "at 5:25"),
            ("in five thirty minutes", "in 5 30 minutes"),
            ("at 5 p.m.", "at 5 pm"),
            ("at five A.M. please", "at 5 am please"),
            ("at five o'clock", "at 5"),
            ("remind me to buy three apples", "remind me to buy three apples"),
            ("remind me to call five people at five", "remind me to call five people at 5"),
            ("at half past five", "at 5:30"),
            ("at quarter past nine", "at 9:15"),
            ("at quarter to six", "at 5:45"),
            ("at quarter to one", "at 12:45"),
            ("at ten past four", "at 4:10"),
            ("at twenty to eight tomorrow", "at 7:40 tomorrow"),
            ("in an hour and a half", "in an hour and a half"),
        ] {
            assert_eq!(digits(said), read, "{said}");
        }
        // "ten to six" with no "at" before it is not a time.
        assert_eq!(digits("it is ten to six people"), "it is ten to six people");
    }

    #[test]
    fn a_part_of_the_day_is_a_time_and_gives_an_hour_its_am_or_pm() {
        for (said, read) in [
            ("remind me tonight to call", "remind me today to call at 8 pm"),
            ("remind me tonight at 8 to call", "remind me today at 8 pm to call"),
            ("remind me this evening to call", "remind me today to call at 6 pm"),
            ("remind me this evening at 7 to call", "remind me today at 7 pm to call"),
            ("remind me tomorrow morning to call", "remind me tomorrow to call at 9 am"),
            ("remind me tomorrow afternoon to call", "remind me tomorrow to call at 3 pm"),
            ("remind me at 7 in the evening to call", "remind me at 7 pm to call"),
            ("remind me at 5 in the morning to call", "remind me at 5 am to call"),
            ("remind me at 1 at night to call", "remind me at 1 am to call"),
            ("remind me on friday evening to call", "remind me on friday to call at 6 pm"),
            ("remind me tomorrow at 9 am to call", "remind me tomorrow at 9 am to call"),
        ] {
            assert_eq!(parts_of_day(said), read, "{said}");
        }
    }

    #[test]
    fn reading_it_again_changes_nothing() {
        for said in [
            "remind me tonight at eight to call Anna",
            "remind me tomorrow morning to take the pills",
            "in an hour and a half",
            "at quarter to six pm",
            "set a timer for twenty five minutes",
            "remind me at 5 p.m. to call",
        ] {
            let once = normalise(said);
            assert_eq!(normalise(&once), once, "{said}");
        }
    }
}
