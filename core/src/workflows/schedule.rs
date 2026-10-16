//! A small 5-field cron parser (`minute hour day-of-month month day-of-week`), in local time.
//!
//! Supports `*`, lists (`1,15`), ranges (`1-5`), steps (`*/10`, `10-40/5`), month and weekday names,
//! and `@daily`, `@hourly`, `@weekly`, `@monthly`. As in standard cron, when both day-of-month and
//! day-of-week are restricted, a day matches if either one does. Times that do not exist (the hour
//! skipped when clocks go forward) are skipped; times that happen twice run once.

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    dom_any: bool,
    dow_any: bool,
}

fn parse_value(text: &str, names: &[&str], offset: u32) -> Result<u32, String> {
    let t = text.to_ascii_lowercase();
    if let Some(i) = names.iter().position(|n| *n == t) {
        return Ok(i as u32 + offset);
    }
    t.parse::<u32>().map_err(|_| format!("'{text}' is not a number or name"))
}

/// Bits set for the values a field allows, and whether the field was a plain `*`.
fn parse_field(field: &str, min: u32, max: u32, names: &[&str], names_offset: u32) -> Result<(u64, bool), String> {
    let mut bits = 0u64;
    let mut any = false;
    for part in field.split(',') {
        if part.is_empty() {
            return Err("an empty item in a list".into());
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| format!("'{s}' is not a valid step"))?),
            None => (part, 1),
        };
        if step == 0 {
            return Err("a step of 0 is not allowed".into());
        }
        let (lo, hi) = if range == "*" {
            if step == 1 {
                any = true;
            }
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (parse_value(a, names, names_offset)?, parse_value(b, names, names_offset)?)
        } else {
            let v = parse_value(range, names, names_offset)?;
            // "5/10" means from 5 to the end in steps of 10.
            if part.contains('/') {
                (v, max)
            } else {
                (v, v)
            }
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("{part} is outside {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, any))
}

impl CronExpr {
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim();
        let expanded = match t {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            other => other,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        if f.len() != 5 {
            return Err("write five fields: minute hour day-of-month month day-of-week (for example \"0 9 * * *\" is every day at 9:00)".into());
        }
        let (minutes, _) = parse_field(f[0], 0, 59, &[], 0).map_err(|e| format!("minute: {e}"))?;
        let (hours, _) = parse_field(f[1], 0, 23, &[], 0).map_err(|e| format!("hour: {e}"))?;
        let (days, dom_any) = parse_field(f[2], 1, 31, &[], 0).map_err(|e| format!("day of month: {e}"))?;
        let (months, _) = parse_field(f[3], 1, 12, &["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"], 1).map_err(|e| format!("month: {e}"))?;
        let (mut wd, dow_any) = parse_field(f[4], 0, 7, &["sun", "mon", "tue", "wed", "thu", "fri", "sat"], 0).map_err(|e| format!("day of week: {e}"))?;
        if wd & (1 << 7) != 0 {
            wd = (wd | 1) & !(1 << 7); // 7 is Sunday too
        }
        Ok(Self { minutes, hours: hours as u32, days: days as u32, months: months as u16, weekdays: wd as u8, dom_any, dow_any })
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        if self.months & (1 << d.month()) == 0 {
            return false;
        }
        let dom = self.days & (1 << d.day()) != 0;
        let dow = self.weekdays & (1 << d.weekday().num_days_from_sunday()) != 0;
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// The first matching minute strictly after `after` (wall-clock time, no zone).
    pub fn next_after_naive(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        let mut t = after.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let limit = t + Duration::days(366 * 5);
        while t < limit {
            if !self.day_matches(t.date()) {
                t = t.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if self.hours & (1 << t.hour()) == 0 {
                t = t.with_minute(0)? + Duration::hours(1);
                continue;
            }
            if self.minutes & (1 << t.minute()) == 0 {
                t += Duration::minutes(1);
                continue;
            }
            return Some(t);
        }
        None
    }

    /// The next run after `after` (unix seconds), in local time. A wall-clock time that does not
    /// exist that day (clocks going forward) is skipped.
    pub fn next_after(&self, after: i64) -> Option<i64> {
        let local = Local.timestamp_opt(after, 0).single().or_else(|| Local.timestamp_opt(after + 3600, 0).single())?;
        let mut cursor = local.naive_local();
        for _ in 0..8 {
            let n = self.next_after_naive(cursor)?;
            match Local.from_local_datetime(&n).earliest() {
                Some(dt) if dt.timestamp() > after => return Some(dt.timestamp()),
                _ => cursor = n, // in a skipped hour: look further on
            }
        }
        None
    }
}

/// The next `count` run times after `after`, for showing a person what a schedule means.
pub fn preview(cron: &str, after: i64, count: usize) -> Result<Vec<i64>, String> {
    let expr = CronExpr::parse(cron)?;
    let mut out = Vec::new();
    let mut t = after;
    while out.len() < count {
        match expr.next_after(t) {
            Some(n) => {
                out.push(n);
                t = n;
            }
            None => break,
        }
    }
    Ok(out)
}
