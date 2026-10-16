//! Turns "tomorrow at 10 AM", "every Monday at 9", "on 2026-10-01 at 10:30" into a structured schedule.
//!
//! Only what can be understood exactly is understood. Anything vague ("later", "soon") returns
//! nothing, and the planner asks. Times are the flow's local wall-clock time; nothing here assumes
//! a server time zone (the caller passes `now` in the zone the flow uses).

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Weekday};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    Once,
    Daily,
    Weekly,
    Monthly,
    Cron,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScheduleSpec {
    pub repeat: Repeat,
    /// "YYYY-MM-DD HH:MM" for a one-time schedule.
    pub when: Option<String>,
    /// A cron expression for anything that repeats.
    pub cron: Option<String>,
    /// A sentence for the person: "tomorrow (2026-10-01) at 10:00".
    pub summary: String,
}

const MONTHS: &[&str] = &["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
const WEEKDAYS: &[(&str, Weekday)] = &[
    ("monday", Weekday::Mon), ("tuesday", Weekday::Tue), ("wednesday", Weekday::Wed), ("thursday", Weekday::Thu),
    ("friday", Weekday::Fri), ("saturday", Weekday::Sat), ("sunday", Weekday::Sun),
];

fn tokens(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')'))
        .map(|t| t.trim_end_matches(['.', '!', '?']).to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// A time of day at token `i`: "10", "10am", "10:30", "10:30pm", "10 am", "noon".
fn time_at(t: &[String], i: usize) -> Option<((u32, u32), usize)> {
    let tok = t.get(i)?.as_str();
    match tok {
        "noon" => return Some(((12, 0), 1)),
        "midnight" => return Some(((0, 0), 1)),
        _ => {}
    }
    let (num, suffix_in) = match tok.find(|c: char| c.is_ascii_alphabetic()) {
        Some(p) => (&tok[..p], &tok[p..]),
        None => (tok, ""),
    };
    let (h, m) = match num.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (num.parse::<u32>().ok()?, 0),
    };
    let (suffix, used) = if !suffix_in.is_empty() {
        (suffix_in, 1)
    } else {
        match t.get(i + 1).map(String::as_str) {
            Some(s @ ("am" | "pm" | "a.m" | "p.m")) => (s, 2),
            _ => ("", 1),
        }
    };
    let pm = suffix.starts_with('p');
    let am = suffix.starts_with('a');
    if (am || pm) && !(1..=12).contains(&h) {
        return None;
    }
    // A bare number is only a time if it says so: a colon, am/pm, or "at" before it (checked by the caller).
    let hour = if pm && h < 12 { h + 12 } else if am && h == 12 { 0 } else { h };
    (hour < 24 && m < 60).then_some(((hour, m), used))
}

fn find_time(t: &[String]) -> Option<(u32, u32)> {
    for i in 0..t.len() {
        let tok = t[i].as_str();
        let after_at = i > 0 && t[i - 1] == "at";
        let explicit = tok.contains(':') || tok.ends_with("am") || tok.ends_with("pm") || matches!(t.get(i + 1).map(String::as_str), Some("am" | "pm")) || matches!(tok, "noon" | "midnight");
        if (after_at || explicit) && !tok.contains('-') {
            if let Some((hm, _)) = time_at(t, i) {
                return Some(hm);
            }
        }
    }
    None
}

fn ordinal(tok: &str) -> Option<u32> {
    let n = tok.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &tok[n.len()..];
    if !suffix.is_empty() && !matches!(suffix, "st" | "nd" | "rd" | "th") {
        return None;
    }
    n.parse::<u32>().ok().filter(|d| (1..=31).contains(d))
}

fn find_date(t: &[String], now: NaiveDateTime) -> Option<(NaiveDate, String)> {
    let today = now.date();
    for (i, tok) in t.iter().enumerate() {
        match tok.as_str() {
            "today" => return Some((today, "today".into())),
            "tomorrow" => return Some((today + Duration::days(1), "tomorrow".into())),
            _ => {}
        }
        if let Ok(d) = NaiveDate::parse_from_str(tok, "%Y-%m-%d") {
            return Some((d, tok.clone()));
        }
        // A month by name, or by its first three letters; it only counts with a day number beside it.
        let month = MONTHS.iter().position(|m| tok == m || (tok.len() == 3 && &m[..3] == tok.as_str()) || (tok == "sept" && *m == "september"));
        if let Some(mi) = month {
            let day = t.get(i + 1).and_then(|n| ordinal(n)).or_else(|| if i > 0 { ordinal(&t[i - 1]) } else { None });
            if let Some(day) = day {
                let mut year = today.year();
                let mut d = NaiveDate::from_ymd_opt(year, mi as u32 + 1, day)?;
                if d < today {
                    year += 1;
                    d = NaiveDate::from_ymd_opt(year, mi as u32 + 1, day)?;
                }
                return Some((d, d.format("%Y-%m-%d").to_string()));
            }
        }
        if let Some((_, wd)) = WEEKDAYS.iter().find(|(n, _)| n == tok) {
            let mut d = today + Duration::days(1);
            while d.weekday() != *wd {
                d += Duration::days(1);
            }
            return Some((d, format!("{} {}", tok, d.format("%Y-%m-%d"))));
        }
    }
    None
}

fn cron_dow(w: Weekday) -> u32 {
    w.num_days_from_sunday()
}

/// Read a schedule from a sentence. `None` if there is nothing exact to read.
pub fn parse(text: &str, now: NaiveDateTime) -> Option<ScheduleSpec> {
    let t = tokens(text);
    let has = |w: &str| t.iter().any(|x| x == w);
    let time = find_time(&t);
    let (h, m) = time.unwrap_or((9, 0));
    let hhmm = format!("{h:02}:{m:02}");
    let time_note = if time.is_some() { String::new() } else { " (no time was given, so 09:00)".to_string() };

    // ---- repeating ---------------------------------------------------------------------------
    let recurring_word = has("every") || has("daily") || has("weekly") || has("monthly") || has("weekdays") || has("weekday") || has("each");
    if recurring_word {
        if has("weekdays") || (has("every") && has("weekday")) {
            return Some(ScheduleSpec { repeat: Repeat::Cron, when: None, cron: Some(format!("{m} {h} * * 1-5")), summary: format!("every weekday at {hhmm}{time_note}") });
        }
        if let Some((name, wd)) = WEEKDAYS.iter().find(|(n, _)| t.iter().any(|x| x == n || *x == format!("{n}s"))) {
            return Some(ScheduleSpec { repeat: Repeat::Weekly, when: None, cron: Some(format!("{m} {h} * * {}", cron_dow(*wd))), summary: format!("every {name} at {hhmm}{time_note}") });
        }
        if has("daily") || (t.windows(2).any(|w| (w[0] == "every" || w[0] == "each") && w[1] == "day")) {
            return Some(ScheduleSpec { repeat: Repeat::Daily, when: None, cron: Some(format!("{m} {h} * * *")), summary: format!("every day at {hhmm}{time_note}") });
        }
        if has("monthly") || t.windows(2).any(|w| (w[0] == "every" || w[0] == "each") && w[1] == "month") {
            // A day of the month is "15th", or a number after "on" / "the"; a bare number is more likely the hour.
            let day = t
                .iter()
                .enumerate()
                .find_map(|(i, x)| {
                    let with_suffix = x.ends_with("st") || x.ends_with("nd") || x.ends_with("rd") || x.ends_with("th");
                    let after_word = i > 0 && matches!(t[i - 1].as_str(), "on" | "the" | "day");
                    (with_suffix || after_word).then(|| ordinal(x)).flatten()
                })
                .unwrap_or(1);
            return Some(ScheduleSpec { repeat: Repeat::Monthly, when: None, cron: Some(format!("{m} {h} {day} * *")), summary: format!("on day {day} of every month at {hhmm}{time_note}") });
        }
        if has("weekly") {
            let dow = cron_dow(now.date().weekday());
            return Some(ScheduleSpec { repeat: Repeat::Weekly, when: None, cron: Some(format!("{m} {h} * * {dow}")), summary: format!("every week at {hhmm}{time_note}") });
        }
    }

    // ---- once --------------------------------------------------------------------------------------
    if let Some((date, label)) = find_date(&t, now) {
        let when = NaiveDateTime::new(date, chrono::NaiveTime::from_hms_opt(h, m, 0)?);
        if when <= now {
            return None; // in the past: ask, do not guess
        }
        return Some(ScheduleSpec { repeat: Repeat::Once, when: Some(when.format("%Y-%m-%d %H:%M").to_string()), cron: None, summary: format!("{label} at {hhmm}{time_note}") });
    }
    // A time on its own: today if it is still ahead, otherwise tomorrow.
    if time.is_some() && (has("schedule") || has("scheduled") || has("at")) {
        let mut when = NaiveDateTime::new(now.date(), chrono::NaiveTime::from_hms_opt(h, m, 0)?);
        let label = if when <= now {
            when += Duration::days(1);
            "tomorrow"
        } else {
            "today"
        };
        return Some(ScheduleSpec { repeat: Repeat::Once, when: Some(when.format("%Y-%m-%d %H:%M").to_string()), cron: None, summary: format!("{label} at {hhmm}") });
    }
    None
}

/// Does the text ask for something to be scheduled or repeated?
pub fn mentions_schedule(text: &str) -> bool {
    let t = tokens(text);
    t.iter().any(|w| matches!(w.as_str(), "schedule" | "scheduled" | "tomorrow" | "daily" | "weekly" | "monthly" | "weekdays"))
        || t.windows(2).any(|w| matches!(w[0].as_str(), "every" | "each") && (w[1] == "day" || w[1] == "week" || w[1] == "month" || w[1] == "weekday" || WEEKDAYS.iter().any(|(n, _)| n == &w[1])))
}


/// How long a repeating schedule goes on, as far as a sentence says: "for 4 weeks", "for a month", "8 times",
/// "until 2026-12-31". Both may be given; whichever comes first stops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Limits {
    pub max_runs: Option<u32>,
    /// The last day, `YYYY-MM-DD`.
    pub end_on: Option<String>,
    /// In words, for the person: "for 4 weeks (until 2026-11-01), 8 times".
    pub summary: String,
}

fn number(word: &str) -> Option<u32> {
    if let Ok(n) = word.parse::<u32>() {
        return Some(n);
    }
    ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve"].iter().position(|w| *w == word).map(|i| i as u32).or(if word == "a" || word == "an" { Some(1) } else { None })
}

pub fn limits(text: &str, now: NaiveDateTime) -> Limits {
    let t = tokens(text);
    let mut out = Limits::default();
    let mut parts = Vec::new();
    for w in t.windows(3) {
        if w[0] == "for" {
            if let Some(n) = number(&w[1]) {
                let unit = w[2].trim_end_matches('s');
                let end = match unit {
                    "day" => now.date().checked_add_signed(Duration::days(n as i64)),
                    "week" => now.date().checked_add_signed(Duration::days(7 * n as i64)),
                    "month" => now.date().checked_add_months(chrono::Months::new(n)),
                    "year" => now.date().checked_add_months(chrono::Months::new(12 * n)),
                    _ => None,
                };
                if let Some(e) = end.filter(|_| n > 0) {
                    out.end_on = Some(e.format("%Y-%m-%d").to_string());
                    parts.push(format!("for {n} {unit}{} (until {})", if n == 1 { "" } else { "s" }, e.format("%Y-%m-%d")));
                }
            }
        }
    }
    for w in t.windows(2) {
        if matches!(w[1].as_str(), "times" | "runs" | "posts" | "time") {
            if let Some(n) = number(&w[0]).filter(|n| (1..=1000).contains(n) && w[0] != "a" && w[0] != "an") {
                out.max_runs = Some(n);
                parts.push(format!("{n} time{}", if n == 1 { "" } else { "s" }));
            }
        }
    }
    if let Some(i) = t.iter().position(|x| x == "until") {
        let rest = &t[i + 1..];
        let iso = rest.first().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
        let day = iso.or_else(|| find_date(rest, now).map(|(d, _)| d));
        if let Some(d) = day.filter(|d| *d >= now.date()) {
            out.end_on = Some(d.format("%Y-%m-%d").to_string());
            parts.push(format!("until {}", d.format("%Y-%m-%d")));
        }
    }
    out.summary = parts.join(", ");
    out
}
