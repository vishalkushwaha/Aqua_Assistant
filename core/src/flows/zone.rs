//! Time zones for schedules: this computer's, a fixed offset (`+05:30`, `UTC`), or an IANA name
//! (`Asia/Kolkata`, `America/New_York`).
//!
//! A schedule says "10:00 in Asia/Kolkata", so all the arithmetic is done on the wall clock of that
//! zone and only turned into a moment (a unix time) at the end. Clocks that go forward skip a wall-clock
//! time (it does not exist, so it is skipped); clocks that go back repeat one (the first is used). Times
//! are stored as unix seconds, which are zone-free, so a stored time never changes meaning.

use crate::workflows::schedule::CronExpr;
use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone};
use chrono_tz::Tz;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Local,
    Fixed(FixedOffset),
    Named(Tz),
}

impl Zone {
    /// "" is this computer's zone. Otherwise `UTC`/`Z`/`GMT`, an offset like `+05:30`, or an IANA name.
    pub fn parse(text: &str) -> Result<Zone, String> {
        let t = text.trim();
        if t.is_empty() {
            return Ok(Zone::Local);
        }
        if t.eq_ignore_ascii_case("utc") || t.eq_ignore_ascii_case("z") || t.eq_ignore_ascii_case("gmt") {
            return Ok(Zone::Fixed(FixedOffset::east_opt(0).expect("zero offset")));
        }
        if let Some(sign) = t.chars().next().filter(|c| matches!(c, '+' | '-')) {
            let rest = &t[1..];
            let (h, m) = rest.split_once(':').unwrap_or((rest, "0"));
            let bad = || format!("\"{t}\" is not a time zone offset (write it like +05:30)");
            let secs = h.parse::<i32>().map_err(|_| bad())? * 3600 + m.parse::<i32>().map_err(|_| bad())? * 60;
            return FixedOffset::east_opt(if sign == '-' { -secs } else { secs }).map(Zone::Fixed).ok_or_else(bad);
        }
        t.parse::<Tz>().map(Zone::Named).map_err(|_| format!("\"{t}\" is not a time zone I know. Use a name such as Asia/Kolkata or America/New_York, an offset such as +05:30, or leave it empty for this computer's"))
    }

    /// How to show it to a person.
    pub fn label(&self) -> String {
        match self {
            Zone::Local => "this computer's time zone".into(),
            Zone::Fixed(o) => {
                let s = o.local_minus_utc();
                if s == 0 {
                    "UTC".into()
                } else {
                    format!("UTC{}{:02}:{:02}", if s < 0 { '-' } else { '+' }, s.abs() / 3600, s.abs() % 3600 / 60)
                }
            }
            Zone::Named(t) => t.name().to_string(),
        }
    }

    /// The moment a wall-clock time in this zone is. `None` if it does not exist (clocks went forward).
    pub fn to_timestamp(&self, naive: NaiveDateTime) -> Option<i64> {
        match self {
            Zone::Local => Local.from_local_datetime(&naive).earliest().map(|d| d.timestamp()),
            Zone::Fixed(o) => o.from_local_datetime(&naive).single().map(|d| d.timestamp()),
            Zone::Named(t) => t.from_local_datetime(&naive).earliest().map(|d| d.timestamp()),
        }
    }

    /// The wall-clock time in this zone at a moment.
    pub fn naive_at(&self, ts: i64) -> Option<NaiveDateTime> {
        let utc = DateTime::from_timestamp(ts, 0)?;
        Some(match self {
            Zone::Local => utc.with_timezone(&Local).naive_local(),
            Zone::Fixed(o) => utc.with_timezone(o).naive_local(),
            Zone::Named(t) => utc.with_timezone(t).naive_local(),
        })
    }

    /// The next time a cron expression fires after `after`, on this zone's wall clock.
    pub fn next_cron(&self, cron: &CronExpr, after: i64) -> Option<i64> {
        let mut cursor = self.naive_at(after)?;
        for _ in 0..8 {
            let n = cron.next_after_naive(cursor)?;
            match self.to_timestamp(n) {
                Some(ts) if ts > after => return Some(ts),
                _ => cursor = n, // a time that does not exist (clocks went forward): look further on
            }
        }
        None
    }

    /// "2026-10-01 10:00 IST" for a person.
    pub fn show(&self, ts: i64) -> String {
        let Some(dt) = DateTime::from_timestamp(ts, 0) else { return String::new() };
        match self {
            Zone::Local => dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string(),
            Zone::Fixed(o) => format!("{} {}", dt.with_timezone(o).format("%Y-%m-%d %H:%M"), self.label()),
            Zone::Named(t) => dt.with_timezone(t).format("%Y-%m-%d %H:%M %Z").to_string(),
        }
    }
}
