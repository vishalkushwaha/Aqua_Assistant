//! Reminders and timers: "remind me to call Anna at 5", "set a timer for ten minutes", "what are my reminders".
//!
//! One engine: a reminder is a **schedule of the flows scheduler** (see `flows::sched`) for a small flow of Aqua's own (one Notify step whose message is the
//! reminder), so it is stored in the database, survives a restart and follows the same rule for a time that was missed while Aqua was off (it is shown when
//! Aqua is back, never dropped silently). When it falls due the notification of the app says the reminder itself, not a report on a workflow run.
//!
//! By voice the sentence is read by rules here, in `handle_voice`, and no model and no tool is involved: the person's own words make a reminder of their
//! own, which is why the voice caller (that may otherwise only look) can do it. The agent and the app use the tools `reminder.add`, `reminder.list` and
//! `reminder.cancel`. A time is kept to the minute (a timer of 30 seconds rings within the minute).
//!
//! What is said is read the way people say it: number words ("twenty five minutes", "quarter past five"), "a.m." and "p.m.", "an hour and a half", "1 hour 30 minutes", "tonight",
//! "tomorrow morning" (see `spoken_time`). An hour said with no am or pm that has passed today is the evening when that is still ahead and a question when it is not; a reminder
//! more than a year ahead is refused with a sentence; "cancel the reminder" cancels the one there is and lists the others, never picking one for the person.

use chrono::{Duration, NaiveDateTime, Timelike};
use regex::Regex;
use std::sync::OnceLock;

/// The flow that holds reminders is the one whose description is this.
pub const MARKER: &str = "aqua:reminders";
pub const MAX_MESSAGE_CHARS: usize = 200;
pub const MAX_REMINDERS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// One time, on the computer's clock.
    Once(NaiveDateTime),
    /// Repeating, as a cron expression.
    Cron(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub message: String,
    pub due: Due,
    /// In words, for what is said back: "in 10 minutes", "today at 17:00".
    pub said: String,
    pub is_timer: bool,
}

/// What a sentence asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    Add(Parsed),
    List,
    CancelAll,
    /// Cancel a reminder (or a timer) that is about this, or the only one there is. When more than one could be meant, they are listed and none is cancelled.
    Cancel { timers: bool, about: Option<String> },
    /// It is about reminders but something is missing: what to say back.
    Missing(&'static str),
    /// It is about reminders and cannot be done as said (too far ahead, or an hour that can mean two): the sentence to say back.
    Tell(String),
    /// Not about reminders at all.
    NotMine,
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("a valid pattern"))
}

const NUMBER_WORDS: [(&str, u32); 17] = [
    ("a", 1), ("an", 1), ("one", 1), ("two", 2), ("three", 3), ("four", 4), ("five", 5), ("six", 6), ("seven", 7), ("eight", 8), ("nine", 9), ("ten", 10),
    ("fifteen", 15), ("twenty", 20), ("thirty", 30), ("forty", 40), ("sixty", 60),
];

fn number(word: &str) -> Option<u32> {
    let w = word.trim().to_ascii_lowercase();
    w.parse().ok().or_else(|| NUMBER_WORDS.iter().find(|(n, _)| *n == w).map(|(_, v)| *v))
}

const AMOUNT: &str = r"(\d+|an?|one|two|three|four|five|six|seven|eight|nine|ten|fifteen|twenty|thirty|forty|sixty)";
const UNIT: &str = r"(seconds?|secs?|minutes?|mins?|hours?|hrs?|days?)";
/// One part of a length of time: "half an hour", "a half" (of the unit before it), "10 minutes", "an hour".
const ONE_PART: &str = r"(?:half\s+an?\s+hour|an?\s+half|(?:\d+|an?|one|two|three|four|five|six|seven|eight|nine|ten|fifteen|twenty|thirty|forty|sixty)\s*(?:seconds?|secs?|minutes?|mins?|hours?|hrs?|days?))";

/// A length of time that is said in parts: "10 minutes", "an hour and a half", "2 hours 30 minutes", "1 hour, 15 minutes".
fn duration_pattern() -> String {
    format!(r"{ONE_PART}(?:\s*(?:,|and)?\s*{ONE_PART})*")
}

/// A reminder cannot be set further ahead than this, in seconds (a year and a day).
pub const FARTHEST_SECONDS: i64 = 366 * 86_400;

/// Why a time could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhenProblem {
    /// Nothing exact could be read from it.
    Unreadable,
    /// It is further ahead than a reminder can be set.
    TooFar,
    /// It can be read in two ways: what to ask.
    Ambiguous(String),
}

impl WhenProblem {
    /// What to say back about it (`None` when nothing could be read: the person is asked when).
    pub fn sentence(&self) -> Option<String> {
        match self {
            WhenProblem::Unreadable => None,
            WhenProblem::TooFar => Some("I can only set a reminder up to a year ahead.".to_string()),
            WhenProblem::Ambiguous(question) => Some(question.clone()),
        }
    }
}

/// "in 10 minutes", "in an hour and a half", "in 2 hours 30 minutes", "in half an hour": how long from now, in words, and where in the text it was said.
fn relative(text: &str) -> Result<Option<(Duration, String, std::ops::Range<usize>)>, WhenProblem> {
    static HALF: OnceLock<Regex> = OnceLock::new();
    static REL: OnceLock<Regex> = OnceLock::new();
    static PART: OnceLock<Regex> = OnceLock::new();
    let Some(whole) = re(&REL, &format!(r"(?i)\bin\s+(?:about\s+|around\s+|exactly\s+)?({})\b", duration_pattern())).captures(text) else {
        // "for half an hour" (a timer says "for"): half an hour.
        return Ok(re(&HALF, r"(?i)\bfor\s+half\s+an?\s+hour\b").find(text).map(|m| (Duration::minutes(30), "in half an hour".to_string(), m.range())));
    };
    let phrase = whole.get(1).map_or("", |m| m.as_str());
    let range = whole.get(0).map_or(0..0, |m| m.range());
    let mut seconds: i64 = 0;
    let mut last_unit: i64 = 0;
    let mut parts = 0;
    let mut said_part = String::new();
    for p in re(&PART, &format!(r"(?i)(half\s+an?\s+hour)|(an?\s+half)|{AMOUNT}\s*{UNIT}")).captures_iter(phrase) {
        parts += 1;
        let (add, unit_seconds) = if p.get(1).is_some() {
            said_part = "in half an hour".to_string();
            (1_800, 3_600)
        } else if p.get(2).is_some() {
            if last_unit == 0 {
                return Err(WhenProblem::Unreadable);
            }
            (last_unit / 2, last_unit)
        } else {
            // A number too long to hold is further ahead than any reminder can be.
            let Some(n) = number(&p[3]).map(i64::from) else { return Err(WhenProblem::TooFar) };
            let (unit_seconds, word) = match p[4].to_ascii_lowercase().chars().next() {
                Some('s') => (1, "second"),
                Some('m') => (60, "minute"),
                Some('h') => (3_600, "hour"),
                _ => (86_400, "day"),
            };
            said_part = format!("in {n} {word}{}", if n == 1 { "" } else { "s" });
            (n.checked_mul(unit_seconds).ok_or(WhenProblem::TooFar)?, unit_seconds)
        };
        seconds = seconds.checked_add(add).ok_or(WhenProblem::TooFar)?;
        last_unit = unit_seconds;
    }
    if parts == 0 {
        return Ok(None);
    }
    if seconds > FARTHEST_SECONDS {
        return Err(WhenProblem::TooFar);
    }
    let said = if parts == 1 && !said_part.is_empty() { said_part } else { format!("in {}", length_in_words(seconds)) };
    Ok(Some((Duration::seconds(seconds), said, range)))
}

/// 5,400 seconds as "1 hour 30 minutes".
fn length_in_words(seconds: i64) -> String {
    let mut left = seconds;
    let mut out = Vec::new();
    for (unit, word) in [(86_400, "day"), (3_600, "hour"), (60, "minute"), (1, "second")] {
        let n = left / unit;
        left %= unit;
        if n > 0 {
            out.push(format!("{n} {word}{}", if n == 1 { "" } else { "s" }));
        }
    }
    if out.is_empty() {
        "0 seconds".into()
    } else {
        out.join(" ")
    }
}

/// The next whole minute at or after `t`, never earlier than a minute from `now` (a reminder cannot be due in the past).
fn to_minute(t: NaiveDateTime, now: NaiveDateTime) -> NaiveDateTime {
    let mut t = t.with_second(0).and_then(|x| x.with_nanosecond(0)).unwrap_or(t) + if t.second() > 0 || t.nanosecond() > 0 { Duration::minutes(1) } else { Duration::zero() };
    let floor = now.with_second(0).and_then(|x| x.with_nanosecond(0)).unwrap_or(now) + Duration::minutes(1);
    if t < floor {
        t = floor;
    }
    t
}

/// "at 5" (an hour with no am or pm, no day and no part of the day said): the hour as said, the minutes, and where the number ends in the text.
fn bare_hour(text: &str) -> Option<(u32, u32, usize)> {
    static BARE: OnceLock<Regex> = OnceLock::new();
    static OTHER: OnceLock<Regex> = OnceLock::new();
    const MONTH: &str = "january|february|march|april|may|june|july|august|september|october|november|december|jan|feb|mar|apr|jun|jul|aug|sept?|oct|nov|dec";
    let named = re(
        &OTHER,
        &format!(
            r"(?i)\b(?:today|tomorrow|tonight|noon|midnight|morning|afternoon|evening|night|every|each|daily|weekly|monthly|weekdays?|monday|tuesday|wednesday|thursday|friday|saturday|sunday|am|pm)\b|\b\d{{4}}-\d{{2}}-\d{{2}}\b|\b(?:{MONTH})\s+\d{{1,2}}\b|\b\d{{1,2}}(?:st|nd|rd|th)?\s+(?:of\s+)?(?:{MONTH})\b"
        ),
    );
    if named.is_match(text) {
        return None;
    }
    let c = re(&BARE, r"(?i)\b(?:at|by|around|before)\s+(\d{1,2})(?::(\d{2}))?\b").captures(text)?;
    let hour: u32 = c[1].parse().ok()?;
    let minutes: u32 = c.get(2).map_or(Some(0), |m| m.as_str().parse().ok())?;
    ((1..=12).contains(&hour) && minutes < 60).then_some((hour, minutes, c.get(0)?.end()))
}

/// A time as a person said it ("in 20 minutes", "tomorrow at 9", "every weekday at 8:30"): when, and how to say it back; or why not. What is said in words ("quarter
/// past five", "tonight at eight", "an hour and a half") is read as the digits it means first. An hour said with no am or pm that has passed today is the evening when that
/// is still ahead today, and a question when it is not (5 in the morning or in the evening?): it is never tomorrow's 05:00 by default.
pub fn read_when(text: &str, now: NaiveDateTime) -> Result<(Due, String), WhenProblem> {
    let text = crate::spoken_time::normalise(text);
    let mut text = text.as_str().to_string();
    if let Some((d, said, _)) = relative(&text)? {
        return Ok((Due::Once(to_minute(now.checked_add_signed(d).ok_or(WhenProblem::TooFar)?, now)), said));
    }
    if let Some((hour, minutes, end)) = bare_hour(&text) {
        let (morning, evening) = (NaiveDateTime::new(now.date(), chrono::NaiveTime::from_hms_opt(hour % 12, minutes, 0).ok_or(WhenProblem::Unreadable)?), NaiveDateTime::new(now.date(), chrono::NaiveTime::from_hms_opt(hour % 12 + 12, minutes, 0).ok_or(WhenProblem::Unreadable)?));
        let shown = if minutes == 0 { hour.to_string() } else { format!("{hour}:{minutes:02}") };
        if hour != 12 && morning <= now {
            if evening > now {
                text.insert_str(end, " pm");
            } else {
                return Err(WhenProblem::Ambiguous(format!("Do you mean {shown} in the morning or {shown} in the evening? Say it again with am or pm.")));
            }
        } else if hour == 12 && evening <= now {
            return Err(WhenProblem::Ambiguous("Do you mean 12 noon or 12 at night? Say it again with am or pm.".into()));
        }
    }
    let spec = crate::flows::schedule_text::parse(&text, now).ok_or(WhenProblem::Unreadable)?;
    match (spec.when, spec.cron) {
        (Some(when), _) => NaiveDateTime::parse_from_str(&when, "%Y-%m-%d %H:%M").ok().map(|t| (Due::Once(t), spec.summary)).ok_or(WhenProblem::Unreadable),
        (None, Some(cron)) => Ok((Due::Cron(cron), spec.summary)),
        _ => Err(WhenProblem::Unreadable),
    }
}

/// [`read_when`] when only whether it could be read matters.
pub fn parse_when(text: &str, now: NaiveDateTime) -> Option<(Due, String)> {
    read_when(text, now).ok()
}

/// The when-phrases taken out of a sentence to leave what to remember.
fn without_when(text: &str) -> String {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = P.get_or_init(|| {
        [
            format!(r"(?i)\b(?:in|for)\s+(?:about\s+|around\s+|exactly\s+)?(?:{})", duration_pattern()),
            r"(?i)\b(?:at|by|around|before)\s+\d{1,2}(?::\d{2})?\s*(?:am|pm)?(?:\b|$)".to_string(),
            r"(?i)\b\d{1,2}(?::\d{2})?\s*(?:am|pm)\b".to_string(),
            r"(?i)\b(?:at\s+)?(?:noon|midnight)\b".to_string(),
            r"(?i)\b(?:tomorrow|today|tonight|this evening|this morning|this afternoon)\b".to_string(),
            r"(?i)\b(?:every\s+(?:other\s+)?|each\s+)(?:day|week|weekday|month|monday|tuesday|wednesday|thursday|friday|saturday|sunday)s?\b".to_string(),
            r"(?i)\b(?:daily|weekly|monthly|weekdays)\b".to_string(),
            r"(?i)\b(?:on\s+|next\s+)(?:monday|tuesday|wednesday|thursday|friday|saturday|sunday)s?\b".to_string(),
            r"(?i)\bon\s+(?:the\s+)?\d{1,2}(?:st|nd|rd|th)?(?:\s+of)?\s+(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\b".to_string(),
            r"(?i)\b(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\s+\d{1,2}(?:st|nd|rd|th)?\b".to_string(),
        ]
        .iter()
        .map(|p| Regex::new(p).expect("a valid pattern"))
        .collect()
    });
    let mut out = text.to_string();
    for p in patterns {
        out = p.replace_all(&out, " ").into_owned();
    }
    let words: Vec<&str> = out.split_whitespace().collect();
    let (mut a, mut b) = (0, words.len());
    let connector = |w: &str| matches!(w.to_ascii_lowercase().trim_matches(|c: char| !c.is_alphabetic()), "at" | "on" | "in" | "by" | "for" | "and" | "then" | "to" | "that" | "about" | "please");
    while a < b && connector(words[a]) {
        a += 1;
    }
    while b > a && connector(words[b - 1]) {
        b -= 1;
    }
    words[a..b].join(" ").trim_matches(|c: char| matches!(c, ',' | '.' | ';' | ':' | '!' | '?')).trim().to_string()
}

/// What a timer says when it is done.
pub const TIMER_MESSAGE: &str = "Your timer is done";

/// What a sentence asks for, by rules. Only sentences that are plainly about reminders and timers are taken; anything else is `NotMine`.
pub fn parse(text: &str, now: NaiveDateTime) -> Ask {
    static REMIND: OnceLock<Regex> = OnceLock::new();
    static TIMER: OnceLock<Regex> = OnceLock::new();
    static WAKE: OnceLock<Regex> = OnceLock::new();
    static LIST: OnceLock<Regex> = OnceLock::new();
    static CANCEL_ALL: OnceLock<Regex> = OnceLock::new();
    static CANCEL_ONE: OnceLock<Regex> = OnceLock::new();
    // What is said in words about a time ("twenty five minutes", "quarter past five", "tonight") is read as digits first.
    let normalised = crate::spoken_time::normalise(text.trim());
    let t = normalised.as_str();
    let lead = r"^\s*(?:(?:hey\s+|ok\s+|okay\s+)?aqua[,\s]+)?(?:please\s+)?(?:can\s+you\s+|could\s+you\s+|would\s+you\s+)?";
    if re(&LIST, &format!(r"(?i){lead}(?:what(?:'s| is| are)\s+(?:my\s+)?(?:reminders?|timers?)|(?:list|show|read)\s+(?:me\s+)?(?:my\s+)?(?:reminders|timers)|(?:do\s+i\s+have|have\s+i\s+got)\s+any\s+(?:reminders|timers)|what\s+reminders\s+do\s+i\s+have)")).is_match(t) {
        return Ask::List;
    }
    if re(&CANCEL_ALL, &format!(r"(?i){lead}(?:cancel|delete|clear|remove)\s+(?:all\s+)?(?:of\s+)?(?:my\s+)?(?:reminders|timers)\s*[.!]?\s*$")).is_match(t) {
        return Ask::CancelAll;
    }
    if let Some(c) = re(&CANCEL_ONE, &format!(r"(?i){lead}(?:cancel|delete|remove)\s+(?:the\s+|my\s+)?(reminder|timer)(?:\s+(?:about|to|for|that)\s+(.+?))?\s*[.!]?\s*$")).captures(t) {
        let about = c.get(2).map(|m| m.as_str().trim().to_string()).filter(|s| !s.is_empty());
        return Ask::Cancel { timers: c[1].eq_ignore_ascii_case("timer"), about };
    }
    // A timer: "set a timer for ten minutes", "timer 5 minutes", "wake me up in an hour and a half".
    let length = duration_pattern();
    let timer = re(&TIMER, &format!(r"(?i){lead}(?:set\s+(?:a\s+|an\s+|the\s+)?)?(?:timer|alarm)\s+(?:for\s+)?({length})\b")).captures(t);
    let wake = re(&WAKE, &format!(r"(?i){lead}wake\s+me(?:\s+up)?\s+in\s+({length})\b")).captures(t);
    if let Some(c) = timer.or(wake) {
        let sentence = format!("in {}", &c[1]);
        return match read_when(&sentence, now) {
            Ok((due, said)) => Ask::Add(Parsed { message: TIMER_MESSAGE.into(), due, said, is_timer: true }),
            Err(WhenProblem::Unreadable) => Ask::Missing("how long the timer should run"),
            Err(problem) => Ask::Tell(problem.sentence().unwrap_or_default()),
        };
    }
    let Some(c) = re(&REMIND, &format!(r"(?i){lead}remind\s+me\b(.*)$")).captures(t) else { return Ask::NotMine };
    let rest = c[1].trim();
    let (due, said) = match read_when(rest, now) {
        Ok(read) => read,
        Err(WhenProblem::Unreadable) => return Ask::Missing("when to remind you"),
        Err(problem) => return Ask::Tell(problem.sentence().unwrap_or_default()),
    };
    let message = without_when(rest);
    if message.is_empty() {
        return Ask::Missing("what to remind you of");
    }
    let message: String = message.chars().take(MAX_MESSAGE_CHARS).collect();
    Ask::Add(Parsed { message, due, said, is_timer: false })
}

/// A reminder as it is listed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReminderView {
    pub id: i64,
    pub message: String,
    /// When it falls due next (seconds since 1970), or `None` when it has run.
    pub due_at: Option<i64>,
    /// In words.
    pub when: String,
}

/// What the reminders need of the flows scheduler (the live one, or a stand-in in tests).
pub trait ReminderApi: Send + Sync {
    fn add(&self, message: &str, due: &Due) -> Result<ReminderView, String>;
    fn list(&self) -> Result<Vec<ReminderView>, String>;
    fn cancel(&self, id: i64) -> Result<bool, String>;
}

fn show_time(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0).map(|u| u.with_timezone(&chrono::Local).format("%a %H:%M").to_string()).unwrap_or_default()
}

/// The reminders listed in a sentence, five at most.
fn listed(l: &[ReminderView]) -> String {
    l.iter().take(5).enumerate().map(|(i, r)| format!("{}, {} {}", i + 1, r.message, r.due_at.map(show_time).unwrap_or_else(|| r.when.clone()))).collect::<Vec<_>>().join("; ")
}

/// The sentence said back for a request made by voice, and what it did. `None` when the sentence is not about reminders.
pub fn answer(api: &dyn ReminderApi, text: &str, now: NaiveDateTime) -> Option<String> {
    match parse(text, now) {
        Ask::NotMine => None,
        Ask::Missing(what) => Some(format!("I need to know {what}.")),
        Ask::Tell(sentence) => Some(sentence),
        Ask::Add(p) => {
            let count = api.list().map(|l| l.len()).unwrap_or(0);
            if count >= MAX_REMINDERS {
                return Some(format!("You already have {MAX_REMINDERS} reminders. Cancel some first."));
            }
            Some(match api.add(&p.message, &p.due) {
                Ok(_) if p.is_timer => format!("Okay, a timer {}. I'll tell you when it is done.", p.said),
                Ok(_) => format!("Okay, I'll remind you to {} {}.", p.message, p.said),
                Err(e) => format!("I could not set that reminder: {e}"),
            })
        }
        Ask::List => Some(match api.list() {
            Ok(l) if l.is_empty() => "You have no reminders.".to_string(),
            Ok(l) => format!("You have {} reminder{}: {}.", l.len(), if l.len() == 1 { "" } else { "s" }, listed(&l)),
            Err(e) => format!("I could not read your reminders: {e}"),
        }),
        Ask::CancelAll => Some(match api.list() {
            Ok(l) if l.is_empty() => "You have no reminders to cancel.".to_string(),
            Ok(l) => {
                let n = l.iter().filter(|r| api.cancel(r.id).unwrap_or(false)).count();
                format!("Cancelled {n} reminder{}.", if n == 1 { "" } else { "s" })
            }
            Err(e) => format!("I could not read your reminders: {e}"),
        }),
        // One is cancelled when it is the one that was meant; when more than one could be, they are listed and nothing is cancelled: "cancel the reminder" never picks.
        Ask::Cancel { timers, about } => Some(match api.list() {
            Ok(l) => {
                let words: Vec<String> = about.iter().flat_map(|a| a.to_ascii_lowercase().split_whitespace().map(str::to_string).collect::<Vec<_>>()).filter(|w| w.len() > 2).collect();
                let kind = if timers { "timer" } else { "reminder" };
                let hits: Vec<ReminderView> = l.iter().filter(|r| (r.message == TIMER_MESSAGE) == timers).filter(|r| words.iter().all(|w| r.message.to_ascii_lowercase().contains(w.as_str()))).cloned().collect();
                match (hits.as_slice(), &about) {
                    ([], Some(about)) => format!("I could not find a {kind} about {about}."),
                    ([], None) => format!("You have no {kind}s to cancel."),
                    ([one], _) => match api.cancel(one.id) {
                        Ok(true) => format!("Cancelled the {kind}{}.", if timers { String::new() } else { format!(" to {}", one.message) }),
                        _ => format!("I could not cancel that {kind}."),
                    },
                    (many, _) => format!("You have {} {kind}s that could be: {}. Tell me which, for example \"cancel the {kind} about\" and a word of it.", many.len(), listed(many)),
                }
            }
            Err(e) => format!("I could not read your reminders: {e}"),
        }),
    }
}

/// Is this a sentence about reminders or timers, one of those `parse` reads? ("I remind you of someone" and "a joke about timers" are not.) The sentence is the person's
/// own to give, so the pipeline answers it before it asks anything else, and whether it was heard as a request or not.
pub fn is_about_reminders(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["remind", "timer", "alarm", "wake me"].iter().any(|w| lower.contains(w)) && !matches!(parse(text, chrono::Local::now().naive_local()), Ask::NotMine)
}

#[cfg(test)]
thread_local! {
    /// A test's own reminders, in place of the live scheduler (which keeps them in the flows database).
    static TEST_API: std::cell::RefCell<Option<Arc<dyn ReminderApi>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with `api` answering for the reminders on this thread (tests).
#[cfg(test)]
pub(crate) fn with_test_api<R>(api: Arc<dyn ReminderApi>, f: impl FnOnce() -> R) -> R {
    let previous = TEST_API.with(|a| a.replace(Some(api)));
    let out = f();
    TEST_API.with(|a| *a.borrow_mut() = previous);
    out
}

/// The voice path: the sentence, answered through the live scheduler. `None` when it is not about reminders.
pub fn handle_voice(text: &str) -> Option<String> {
    // The cheap check first: a sentence with none of these words is nobody's reminder.
    let lower = text.to_ascii_lowercase();
    if !["remind", "timer", "alarm", "wake me"].iter().any(|w| lower.contains(w)) {
        return None;
    }
    let now = chrono::Local::now().naive_local();
    #[cfg(test)]
    if let Some(api) = TEST_API.with(|a| a.borrow().clone()) {
        return answer(api.as_ref(), text, now);
    }
    answer(&Live, text, now)
}

// ---- the tools -----------------------------------------------------------------------------------------------------------------------

use aqua_tool_host::{Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider};
use serde_json::{json, Value};
use std::sync::Arc;

/// `reminder.add(message, when)`, `reminder.list()` and `reminder.cancel(id)` for the agent and the app. `when` is said as a person says it.
pub struct ReminderProvider(pub Arc<dyn ReminderApi>);

impl ToolProvider for ReminderProvider {
    fn id(&self) -> &str {
        "reminders"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let m = |name: &str, description: &str, classes: Vec<PermClass>, schema: Value, read_only: bool| ToolManifest {
            name: name.into(),
            title: name.into(),
            description: description.into(),
            classes,
            default_decision: Decision::Auto,
            input_schema: schema,
            timeout_default_ms: 10_000,
            timeout_max_ms: 20_000,
            annotations: Annotations { read_only, destructive: false, idempotent: read_only, open_world: false },
            untrusted_output: false,
        };
        vec![
            m(
                "reminder.add",
                "Remind the user of something at a time, said as people say it: in 20 minutes, tomorrow at 9, every weekday at 8:30.",
                vec![PermClass::Write],
                json!({"type": "object", "properties": {"message": {"type": "string", "minLength": 1, "maxLength": MAX_MESSAGE_CHARS}, "when": {"type": "string", "minLength": 2, "maxLength": 100}}, "required": ["message", "when"]}),
                false,
            ),
            m("reminder.list", "List the user's reminders and timers that have not gone off yet.", vec![PermClass::Read], json!({"type": "object", "properties": {}}), true),
            m("reminder.cancel", "Cancel one reminder by its number (see reminder.list).", vec![PermClass::Write], json!({"type": "object", "properties": {"id": {"type": "integer", "minimum": 1}}, "required": ["id"]}), false),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let summary = match tool {
            "reminder.add" => format!("Set a reminder: {} ({})", args.get("message").and_then(|v| v.as_str()).unwrap_or("").chars().take(80).collect::<String>(), args.get("when").and_then(|v| v.as_str()).unwrap_or("")),
            "reminder.cancel" => format!("Cancel reminder {}", args.get("id").and_then(|v| v.as_i64()).unwrap_or(0)),
            _ => "List the reminders".into(),
        };
        Ok(Classification { classes: vec![], target: "reminders".into(), summary, target_allowed: true })
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, _ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let api = self.0.clone();
        let tool = tool.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> Result<ProviderOutput, ToolError> {
                let backend = |e: String| ToolError::new(ErrorCode::BackendError, e);
                match tool.as_str() {
                    "reminder.add" => {
                        let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
                        let when = args.get("when").and_then(|v| v.as_str()).unwrap_or("");
                        let (due, said) = read_when(when, chrono::Local::now().naive_local()).map_err(|problem| {
                            ToolError::new(ErrorCode::InvalidArguments, problem.sentence().unwrap_or_else(|| format!("could not read the time {when:?}: say it like \"in 20 minutes\", \"tomorrow at 9\" or \"every weekday at 8:30\"")))
                        })?;
                        if message.is_empty() {
                            return Err(ToolError::new(ErrorCode::InvalidArguments, "message is what to remind the user of"));
                        }
                        if api.list().map(|l| l.len()).unwrap_or(0) >= MAX_REMINDERS {
                            return Err(ToolError::new(ErrorCode::InvalidArguments, format!("there are {MAX_REMINDERS} reminders already: cancel some first")));
                        }
                        let r = api.add(&message, &due).map_err(backend)?;
                        Ok(ProviderOutput { data: json!({"reminder": r}), text: format!("Reminder {} set: {} ({said}).", r.id, r.message), ..Default::default() })
                    }
                    "reminder.cancel" => {
                        let id = args.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                        if !api.cancel(id).map_err(backend)? {
                            return Err(ToolError::new(ErrorCode::NotFound, format!("there is no reminder {id} (see reminder.list)")));
                        }
                        Ok(ProviderOutput { data: json!({"cancelled": id}), text: format!("Cancelled reminder {id}."), ..Default::default() })
                    }
                    _ => {
                        let list = api.list().map_err(backend)?;
                        let text = if list.is_empty() {
                            "There are no reminders.".to_string()
                        } else {
                            list.iter().map(|r| format!("{}: {} ({})", r.id, r.message, r.due_at.map(show_time).unwrap_or_else(|| r.when.clone()))).collect::<Vec<_>>().join("\n")
                        };
                        Ok(ProviderOutput { data: json!({"reminders": list}), text, ..Default::default() })
                    }
                }
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the reminders task stopped unexpectedly: {e}")))?
        })
    }
}

// ---- the live scheduler ------------------------------------------------------------------------------------------------------------

pub struct Live;

fn owner() -> &'static str {
    crate::flows::model::current_owner()
}

/// Looking for the flow of reminders and making it are one step: two first reminders at once (a person says two, or an agent and the person) would each find none and
/// make one, and the reminders would be spread over two flows.
static FLOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The flow that holds reminders: found by its marker, or made (a trigger and a Notify step that says `{{vars.message}}`).
fn reminder_flow() -> Result<i64, String> {
    use crate::flows::model::*;
    let _one_at_a_time = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = crate::flows::services::global().flows();
    for s in store.list(owner())? {
        if let Some(f) = store.get(owner(), s.id)? {
            if f.def.description == MARKER {
                return Ok(f.id);
            }
        }
    }
    let mut def = FlowDef {
        name: "Reminders".into(),
        description: MARKER.into(),
        nodes: vec![
            Node::new("start", NodeKind::Trigger, "Start").with_config(serde_json::json!({ "trigger_type": "manual" })),
            Node::new("say", NodeKind::Notification, "Tell the person").with_config(serde_json::json!({ "channel": "in_app", "message": "{{vars.message}}" })),
        ],
        edges: vec![Edge::new("start", "say", "out")],
        variables: vec![VariableDef { name: "message".into(), label: "What to remind you of".into(), kind: VarKind::Text, required: true, default: None, choices: vec![] }],
        ..Default::default()
    };
    def.normalize();
    Ok(store.create(owner(), def, "aqua", "the flow of reminders", chrono::Utc::now().timestamp())?.id)
}

fn view_of(s: &crate::flows::sched::model::Schedule) -> ReminderView {
    ReminderView { id: s.id, message: s.inputs.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string(), due_at: s.next_run_at, when: s.describe() }
}

impl ReminderApi for Live {
    fn add(&self, message: &str, due: &Due) -> Result<ReminderView, String> {
        use crate::flows::sched::model::{Campaign, Misfire, NotifyPrefs, ScheduleInput, When};
        let flow = reminder_flow()?;
        let when = match due {
            Due::Once(t) => When::Once { when: t.format("%Y-%m-%d %H:%M").to_string() },
            Due::Cron(c) => When::Cron { cron: c.clone() },
        };
        let mut inputs = serde_json::Map::new();
        inputs.insert("message".into(), serde_json::Value::String(message.chars().take(MAX_MESSAGE_CHARS).collect()));
        let input = ScheduleInput {
            name: format!("Reminder: {}", message.chars().take(60).collect::<String>()),
            when,
            timezone: String::new(),
            inputs,
            // A reminder that fell due while Aqua was off is said when Aqua is back.
            misfire: Misfire::RunNow,
            // In the app's list always, and said aloud by the app's player when it is open (the `voice` channel, `flows/notify_voice.rs`; T6-06.9).
            notify: NotifyPrefs { on_start: false, on_success: true, on_failure: true, on_recovered: false, on_needs_you: true, on_each_step: false, channels: vec!["in_app".into(), crate::flows::notify_voice::CHANNEL_ID.into()] },
            enabled: true,
            campaign: Campaign::default(),
        };
        let s = crate::flows::services::global().schedules().create(owner(), flow, &input, chrono::Utc::now().timestamp())?;
        Ok(view_of(&s))
    }

    fn list(&self) -> Result<Vec<ReminderView>, String> {
        let flow = reminder_flow()?;
        let store = crate::flows::services::global().schedules();
        let all = store.list(owner(), Some(flow))?;
        // A reminder that has gone off is kept for a day, and then removed: the database does not grow with every reminder ever set.
        let now = chrono::Utc::now().timestamp();
        for done in all.iter().filter(|s| s.next_run_at.is_none() && s.last_run_at > 0 && now - s.last_run_at > 86_400) {
            let _ = store.delete(owner(), done.id);
        }
        let mut out: Vec<ReminderView> = all.iter().filter(|s| s.enabled && s.next_run_at.is_some()).map(view_of).collect();
        out.sort_by_key(|r| r.due_at);
        Ok(out)
    }

    fn cancel(&self, id: i64) -> Result<bool, String> {
        let flow = reminder_flow()?;
        let store = crate::flows::services::global().schedules();
        match store.get(owner(), id)? {
            Some(s) if s.flow_id == flow => store.delete(owner(), id),
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::sync::Mutex;

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, mi, 0).unwrap()
    }

    /// Monday 2026-10-05, 14:07.
    fn now() -> NaiveDateTime {
        at(2026, 10, 5, 14, 7)
    }

    fn add(text: &str) -> Parsed {
        match parse(text, now()) {
            Ask::Add(p) => p,
            other => panic!("{text:?} gave {other:?}"),
        }
    }

    #[test]
    fn a_reminder_is_read_into_what_to_remember_and_when() {
        let p = add("remind me to call Anna at 5pm");
        assert_eq!((p.message.as_str(), p.due.clone()), ("call Anna", Due::Once(at(2026, 10, 5, 17, 0))));
        let p = add("Aqua, please remind me to check the oven in 10 minutes");
        assert_eq!((p.message.as_str(), p.due, p.said.as_str()), ("check the oven", Due::Once(at(2026, 10, 5, 14, 17)), "in 10 minutes"));
        let p = add("remind me in an hour to take the pizza out");
        assert_eq!((p.message.as_str(), p.due), ("take the pizza out", Due::Once(at(2026, 10, 5, 15, 7))));
        let p = add("remind me tomorrow at 9 to send the invoice");
        assert_eq!((p.message.as_str(), p.due), ("send the invoice", Due::Once(at(2026, 10, 6, 9, 0))));
        let p = add("remind me about the dentist on friday at 3 pm");
        assert_eq!((p.message.as_str(), p.due), ("the dentist", Due::Once(at(2026, 10, 9, 15, 0))));
        let p = add("remind me every weekday at 8:30 to water the plants");
        assert_eq!((p.message.as_str(), p.due), ("water the plants", Due::Cron("30 8 * * 1-5".into())));
        let p = add("remind me at 9am to stretch");
        assert_eq!(p.due, Due::Once(at(2026, 10, 6, 9, 0)), "9 am has passed today, so it is tomorrow");
        let p = add("can you remind me in half an hour to call back");
        assert_eq!((p.message.as_str(), p.due), ("call back", Due::Once(at(2026, 10, 5, 14, 37))));
    }

    #[test]
    fn a_timer_is_a_reminder_of_its_own_and_a_short_one_rings_within_the_minute() {
        let p = add("set a timer for ten minutes");
        assert!(p.is_timer && p.message == "Your timer is done" && p.due == Due::Once(at(2026, 10, 5, 14, 17)) && p.said == "in 10 minutes", "{p:?}");
        assert_eq!(add("timer 5 minutes").due, Due::Once(at(2026, 10, 5, 14, 12)));
        assert_eq!(add("wake me up in two hours").due, Due::Once(at(2026, 10, 5, 16, 7)));
        let p = add("set a timer for 30 seconds");
        assert_eq!(p.due, Due::Once(at(2026, 10, 5, 14, 8)), "never earlier than the next minute: {p:?}");
        assert_eq!(parse("set a timer", now()), Ask::NotMine, "no length: it is not a timer request this can read");
    }

    #[test]
    fn something_missing_is_asked_for_and_other_sentences_are_not_taken() {
        assert_eq!(parse("remind me to call Anna", now()), Ask::Missing("when to remind you"));
        assert_eq!(parse("remind me at 5pm", now()), Ask::Missing("what to remind you of"));
        assert_eq!(parse("remind me", now()), Ask::Missing("when to remind you"));
        for other in ["what time is it", "open notepad", "I remind you of someone", "tell me a joke about timers", "do not remind me of that", "set the volume to ten"] {
            assert_eq!(parse(other, now()), Ask::NotMine, "{other}");
        }
    }

    #[test]
    fn the_questions_and_the_cancellations_are_read() {
        for list in ["what are my reminders", "What's my reminders", "list my reminders", "do I have any reminders", "Aqua, what reminders do I have?"] {
            assert_eq!(parse(list, now()), Ask::List, "{list}");
        }
        assert_eq!(parse("cancel all my reminders", now()), Ask::CancelAll);
        assert_eq!(parse("cancel the reminder about the dentist", now()), Ask::Cancel { timers: false, about: Some("the dentist".into()) });
        assert_eq!(parse("cancel my timer", now()), Ask::Cancel { timers: true, about: None });
        assert_eq!(parse("cancel the reminder", now()), Ask::Cancel { timers: false, about: None });
        assert_eq!(parse("delete the reminder to call Anna", now()), Ask::Cancel { timers: false, about: Some("call Anna".into()) });
    }

    fn once(d: u32, h: u32, m: u32) -> Due {
        Due::Once(at(2026, 10, d, h, m))
    }

    fn cron(c: &str) -> Due {
        Due::Cron(c.into())
    }

    /// What people say, and what it means: each way of saying when is tried before and after what to remember, at Monday 2026-10-05, 14:07.
    #[test]
    fn spoken_forms_of_a_time_are_read_into_what_to_remember_and_when_wherever_they_stand_in_the_sentence() {
        let when: Vec<(&str, Due)> = vec![
            // a clock time, in digits and in words
            ("at 5pm", once(5, 17, 0)),
            ("at 5 pm", once(5, 17, 0)),
            ("at 5 p.m.", once(5, 17, 0)),
            ("at 5 P.M.", once(5, 17, 0)),
            ("at five pm", once(5, 17, 0)),
            ("at five p.m.", once(5, 17, 0)),
            ("at five o'clock", once(5, 17, 0)),
            ("at 5", once(5, 17, 0)),
            ("at 5:30", once(5, 17, 30)),
            ("at half past five", once(5, 17, 30)),
            ("at quarter past five", once(5, 17, 15)),
            ("at quarter to six", once(5, 17, 45)),
            ("at ten past five", once(5, 17, 10)),
            ("at twenty to six", once(5, 17, 40)),
            ("at five thirty", once(5, 17, 30)),
            ("at five thirty pm", once(5, 17, 30)),
            ("at 17:00", once(5, 17, 0)),
            ("at 17", once(5, 17, 0)),
            ("at noon", once(6, 12, 0)),
            ("at midnight", once(6, 0, 0)),
            ("at 9 am", once(6, 9, 0)),
            ("at 9", once(5, 21, 0)),
            ("at nine", once(5, 21, 0)),
            ("at 7 in the evening", once(5, 19, 0)),
            ("at 7:15 in the evening", once(5, 19, 15)),
            ("at 7 in the morning", once(6, 7, 0)),
            ("at 5 in the afternoon", once(5, 17, 0)),
            ("at 11 at night", once(5, 23, 0)),
            ("at 1 at night", once(6, 1, 0)),
            // a part of the day
            ("tonight at 8", once(5, 20, 0)),
            ("tonight at eight", once(5, 20, 0)),
            ("tonight", once(5, 20, 0)),
            ("this evening", once(5, 18, 0)),
            ("this evening at 7", once(5, 19, 0)),
            ("this afternoon", once(5, 15, 0)),
            ("tomorrow morning", once(6, 9, 0)),
            ("tomorrow afternoon", once(6, 15, 0)),
            ("tomorrow evening", once(6, 18, 0)),
            ("tomorrow night", once(6, 20, 0)),
            ("tomorrow at 9", once(6, 9, 0)),
            ("tomorrow at nine", once(6, 9, 0)),
            ("tomorrow at nine thirty", once(6, 9, 30)),
            ("tomorrow at 9:30 am", once(6, 9, 30)),
            ("tomorrow at 9 pm", once(6, 21, 0)),
            ("tomorrow at noon", once(6, 12, 0)),
            ("on friday at 3 pm", once(9, 15, 0)),
            ("on friday evening", once(9, 18, 0)),
            ("on friday morning", once(9, 9, 0)),
            // how long from now
            ("in 10 minutes", once(5, 14, 17)),
            ("in ten minutes", once(5, 14, 17)),
            ("in twenty minutes", once(5, 14, 27)),
            ("in twenty five minutes", once(5, 14, 32)),
            ("in twenty-five minutes", once(5, 14, 32)),
            ("in an hour", once(5, 15, 7)),
            ("in one hour", once(5, 15, 7)),
            ("in two hours", once(5, 16, 7)),
            ("in half an hour", once(5, 14, 37)),
            ("in an hour and a half", once(5, 15, 37)),
            ("in 1 hour 30 minutes", once(5, 15, 37)),
            ("in 1 hour and 30 minutes", once(5, 15, 37)),
            ("in two hours and thirty minutes", once(5, 16, 37)),
            ("in 90 minutes", once(5, 15, 37)),
            ("in 2 hours 15 minutes", once(5, 16, 22)),
            ("in a day", once(6, 14, 7)),
            ("in two days", once(7, 14, 7)),
            ("in 3 days", once(8, 14, 7)),
            ("in 45 seconds", once(5, 14, 8)),
            ("in about 10 minutes", once(5, 14, 17)),
            ("in around five minutes", once(5, 14, 12)),
            ("in 365 days", Due::Once(at(2027, 10, 5, 14, 7))),
        ];
        let mut wrong = Vec::new();
        let mut tried = 0;
        for (said, due) in &when {
            for sentence in [format!("remind me to call Anna {said}"), format!("remind me {said} to call Anna")] {
                tried += 1;
                match parse(&sentence, now()) {
                    Ask::Add(p) if p.message == "call Anna" && p.due == *due && !p.is_timer => {}
                    other => wrong.push(format!("{sentence:?}: wanted \"call Anna\" at {due:?}, got {other:?}")),
                }
            }
        }
        // Timers and "wake me": the message is the timer's own.
        for (said, due) in [
            ("set a timer for ten minutes", once(5, 14, 17)),
            ("set a timer for twenty five minutes", once(5, 14, 32)),
            ("set a timer for an hour and a half", once(5, 15, 37)),
            ("set a timer for 1 hour 15 minutes", once(5, 15, 22)),
            ("timer 5 minutes", once(5, 14, 12)),
            ("timer for five minutes", once(5, 14, 12)),
            ("set a timer for half an hour", once(5, 14, 37)),
            ("wake me up in an hour", once(5, 15, 7)),
            ("wake me up in an hour and a half", once(5, 15, 37)),
            ("set an alarm for ten minutes", once(5, 14, 17)),
            ("Aqua, set a timer for 2 minutes", once(5, 14, 9)),
            ("please set a timer for three minutes", once(5, 14, 10)),
            ("can you set a timer for forty five minutes", once(5, 14, 52)),
        ] {
            tried += 1;
            match parse(said, now()) {
                Ask::Add(p) if p.message == TIMER_MESSAGE && p.due == due && p.is_timer => {}
                other => wrong.push(format!("{said:?}: wanted a timer at {due:?}, got {other:?}")),
            }
        }
        // Repeating, and messages that have numbers of their own.
        for (said, message, due) in [
            ("remind me every weekday at 9 to stand up", "stand up", cron("0 9 * * 1-5")),
            ("remind me every weekday at nine to stand up", "stand up", cron("0 9 * * 1-5")),
            ("remind me every day at 8:30 pm to take the pills", "take the pills", cron("30 20 * * *")),
            ("remind me every monday at 10 am to write the report", "write the report", cron("0 10 * * 1")),
            ("remind me every monday at ten am to write the report", "write the report", cron("0 10 * * 1")),
            ("remind me to water the plants every friday at 6 pm", "water the plants", cron("0 18 * * 5")),
            ("remind me daily at 7 am to stretch", "stretch", cron("0 7 * * *")),
            ("remind me to buy three apples at five", "buy three apples", once(5, 17, 0)),
            ("remind me to call five people at 5 pm", "call five people", once(5, 17, 0)),
            ("remind me to take ten pills in ten minutes", "take ten pills", once(5, 14, 17)),
            ("remind me to call mom in an hour", "call mom", once(5, 15, 7)),
        ] {
            tried += 1;
            match parse(said, now()) {
                Ask::Add(p) if p.message == message && p.due == due => {}
                other => wrong.push(format!("{said:?}: wanted {message:?} at {due:?}, got {other:?}")),
            }
        }
        assert!(tried >= 160, "{tried}");
        assert!(wrong.is_empty(), "{} of {tried} wrong:\n{}", wrong.len(), wrong.join("\n"));
    }

    #[test]
    fn what_is_said_back_for_a_length_of_time_names_its_parts() {
        let said = |text: &str| match parse(text, now()) {
            Ask::Add(p) => p.said,
            other => panic!("{text:?} gave {other:?}"),
        };
        assert_eq!(said("remind me to call Anna in 10 minutes"), "in 10 minutes");
        assert_eq!(said("remind me to call Anna in an hour"), "in 1 hour");
        assert_eq!(said("remind me to call Anna in half an hour"), "in half an hour");
        assert_eq!(said("remind me to call Anna in an hour and a half"), "in 1 hour 30 minutes");
        assert_eq!(said("remind me to call Anna in 90 minutes"), "in 90 minutes");
        assert_eq!(said("remind me to call Anna in 2 hours 15 minutes"), "in 2 hours 15 minutes");
        assert_eq!(said("remind me to call Anna in 2 days"), "in 2 days");
        assert_eq!(said("remind me to call Anna at 5"), "today at 17:00");
    }

    #[test]
    fn an_hour_that_can_mean_two_things_and_a_time_too_far_ahead_are_answered_with_a_sentence_and_nothing_is_set() {
        // At 20:00 "at 5" is 05:00 tomorrow or 17:00 tomorrow: asked, never tomorrow's 05:00 by default.
        match parse("remind me to call Anna at 5", at(2026, 10, 5, 20, 0)) {
            Ask::Tell(q) => assert!(q.contains("5 in the morning or 5 in the evening") && q.contains("am or pm"), "{q}"),
            other => panic!("{other:?}"),
        }
        match parse("remind me to call Anna at 7:30", at(2026, 10, 5, 22, 0)) {
            Ask::Tell(q) => assert!(q.contains("7:30 in the morning or 7:30 in the evening"), "{q}"),
            other => panic!("{other:?}"),
        }
        // When the morning of that hour is still ahead, it is that.
        match parse("remind me to call Anna at 7", at(2026, 10, 5, 6, 0)) {
            Ask::Add(p) => assert_eq!(p.due, once(5, 7, 0)),
            other => panic!("{other:?}"),
        }
        // 12 with neither noon nor midnight to come today is asked.
        match parse("remind me to call Anna at 12", now()) {
            Ask::Tell(q) => assert!(q.contains("noon") && q.contains("night"), "{q}"),
            other => panic!("{other:?}"),
        }
        // Said with am or pm, with a day or with a part of the day, nothing is asked.
        for fine in ["at 5 am", "at 5 pm", "tomorrow at 5", "at 5 in the morning"] {
            assert!(matches!(parse(&format!("remind me to call Anna {fine}"), at(2026, 10, 5, 20, 0)), Ask::Add(_)), "{fine}");
        }
        let far = "I can only set a reminder up to a year ahead.";
        for too_far in ["in 100000000 days", "in 400 days", "in 99999999999 days", "in 9000 hours"] {
            assert_eq!(parse(&format!("remind me {too_far} to call Anna"), now()), Ask::Tell(far.into()), "{too_far}");
        }
        assert_eq!(parse("set a timer for 500 days", now()), Ask::Tell(far.into()));
        assert!(matches!(parse("remind me in 366 days to call Anna", now()), Ask::Add(_)), "a year and a day is the most");
        let api = Fake::default();
        assert_eq!(answer(&api, "remind me in 100000000 days to call Anna", now()).unwrap(), far);
        assert!(api.list().unwrap().is_empty(), "nothing was set");
        // The tool that the agent uses says the same, as an error it can read.
        assert_eq!(read_when("in 100000000 days", now()), Err(WhenProblem::TooFar));
        assert_eq!(read_when("sometime", now()), Err(WhenProblem::Unreadable));
    }

    #[test]
    fn cancel_the_reminder_cancels_the_one_there_is_lists_the_others_and_never_picks_for_the_person() {
        let api = Fake::default();
        let say = |t: &str| answer(&api, t, now()).unwrap();
        say("remind me to call Anna at 5 pm");
        say("remind me to water the plants tomorrow at 9");
        say("set a timer for ten minutes");
        // Two reminders could be meant: they are listed and nothing is cancelled.
        let reply = say("cancel the reminder");
        assert!(reply.starts_with("You have 2 reminders that could be: 1, call Anna") && reply.contains("water the plants"), "{reply}");
        assert_eq!(api.list().unwrap().len(), 3, "nothing was cancelled");
        // A word of it says which.
        assert_eq!(say("cancel the reminder about plants"), "Cancelled the reminder to water the plants.");
        // One is left, and it is the one "the reminder" means; the timer is a timer.
        assert_eq!(say("cancel the reminder"), "Cancelled the reminder to call Anna.");
        assert_eq!(api.list().unwrap().len(), 1);
        assert_eq!(say("cancel the reminder"), "You have no reminders to cancel.");
        assert_eq!(say("cancel my timer"), "Cancelled the timer.");
        assert_eq!(say("cancel the timer"), "You have no timers to cancel.");
    }


    #[derive(Default)]
    struct Fake {
        items: Mutex<Vec<ReminderView>>,
    }

    impl ReminderApi for Fake {
        fn add(&self, message: &str, due: &Due) -> Result<ReminderView, String> {
            let mut v = self.items.lock().unwrap();
            let id = v.len() as i64 + 1;
            let r = ReminderView { id, message: message.into(), due_at: Some(1_000 + id), when: format!("{due:?}") };
            v.push(r.clone());
            Ok(r)
        }
        fn list(&self) -> Result<Vec<ReminderView>, String> {
            Ok(self.items.lock().unwrap().clone())
        }
        fn cancel(&self, id: i64) -> Result<bool, String> {
            let mut v = self.items.lock().unwrap();
            let before = v.len();
            v.retain(|r| r.id != id);
            Ok(v.len() != before)
        }
    }

    #[test]
    fn by_voice_a_reminder_is_set_listed_and_cancelled_with_a_sentence_said_back_each_time() {
        let api = Fake::default();
        let say = |t: &str| answer(&api, t, now()).unwrap();
        assert_eq!(say("remind me to call Anna at 5pm"), "Okay, I'll remind you to call Anna today at 17:00.");
        assert_eq!(say("set a timer for ten minutes"), "Okay, a timer in 10 minutes. I'll tell you when it is done.");
        assert!(say("what are my reminders").starts_with("You have 2 reminders: 1, call Anna"));
        assert_eq!(say("cancel the reminder about call anna"), "Cancelled the reminder to call Anna.");
        assert_eq!(say("cancel the reminder about the dentist"), "I could not find a reminder about the dentist.");
        assert_eq!(say("cancel all my reminders"), "Cancelled 1 reminder.");
        assert_eq!(say("what are my reminders"), "You have no reminders.");
        assert_eq!(say("remind me to call Anna"), "I need to know when to remind you.");
        assert!(answer(&api, "what time is it", now()).is_none());
        // The cheap check: nothing about reminders, nothing is asked of the scheduler.
        assert!(handle_voice("open the notepad please").is_none());
    }

    #[test]
    fn the_tools_set_list_and_cancel_through_the_host_and_a_time_that_cannot_be_read_is_an_error_that_shows_how_to_say_it() {
        use aqua_tool_host::*;
        let dir = std::env::temp_dir().join(format!("aqua_reminder_tools_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let host = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Developer)), Arc::new(AuditLedger::open(dir.join("a.jsonl"))));
        let api = Arc::new(Fake::default());
        host.register(Arc::new(ReminderProvider(api.clone()))).unwrap();
        let call = |tool: &str, args: Value| {
            let mut ctx = RequestContext::new(Caller::Agent);
            ctx.agent_run_id = "r".into();
            host.call_blocking(ToolRequest { tool: tool.into(), arguments: args, context: ctx })
        };
        let added = call("reminder.add", json!({"message": "call   Anna", "when": "in 20 minutes"}));
        assert!(added.ok && added.text.contains("Reminder 1 set: call Anna (in 20 minutes)"), "{}", added.text);
        let bad = call("reminder.add", json!({"message": "x", "when": "sometime"}));
        assert_eq!(bad.error.unwrap().code, ErrorCode::InvalidArguments);
        assert!(bad.text.contains("tomorrow at 9"), "{}", bad.text);
        assert!(call("reminder.list", json!({})).text.contains("1: call Anna"));
        assert_eq!(call("reminder.cancel", json!({"id": 7})).error.unwrap().code, ErrorCode::NotFound);
        assert!(call("reminder.cancel", json!({"id": 1})).ok && call("reminder.list", json!({})).text.contains("no reminders"));
        // A person who is only talking sets a reminder through the sentence, not through these tools (a voice caller may only look).
        let mut voice = RequestContext::new(Caller::Voice);
        voice.agent_run_id = "v".into();
        assert!(!host.call_blocking(ToolRequest { tool: "reminder.add".into(), arguments: json!({"message": "x", "when": "in 5 minutes"}), context: voice }).ok);
    }

    #[test]
    fn there_is_a_limit_to_how_many_reminders_are_kept() {
        let api = Fake::default();
        for i in 0..MAX_REMINDERS {
            api.add(&format!("r{i}"), &Due::Once(now())).unwrap();
        }
        assert!(answer(&api, "remind me to one more in 5 minutes", now()).unwrap().contains("already have 100"));
    }
}
