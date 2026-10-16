use super::schedule::*;
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};

fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 0).unwrap()
}

fn next(cron: &str, after: NaiveDateTime) -> NaiveDateTime {
    CronExpr::parse(cron).unwrap().next_after_naive(after).unwrap()
}

#[test]
fn every_day_at_nine_runs_today_if_it_has_not_passed_and_tomorrow_if_it_has() {
    assert_eq!(next("0 9 * * *", at(2026, 3, 10, 8, 59)), at(2026, 3, 10, 9, 0));
    assert_eq!(next("0 9 * * *", at(2026, 3, 10, 9, 0)), at(2026, 3, 11, 9, 0), "strictly after");
    assert_eq!(next("0 9 * * *", at(2026, 12, 31, 9, 30)), at(2027, 1, 1, 9, 0), "across the year");
}

#[test]
fn steps_lists_ranges_and_names_work() {
    assert_eq!(next("*/15 * * * *", at(2026, 3, 10, 10, 7)), at(2026, 3, 10, 10, 15));
    assert_eq!(next("0 9,17 * * *", at(2026, 3, 10, 9, 0)), at(2026, 3, 10, 17, 0));
    assert_eq!(next("10-40/10 * * * *", at(2026, 3, 10, 10, 11)), at(2026, 3, 10, 10, 20));
    // 2026-03-13 is a Friday: the next weekday morning is Monday the 16th.
    assert_eq!(next("30 8 * * mon-fri", at(2026, 3, 13, 9, 0)), at(2026, 3, 16, 8, 30));
    assert_eq!(next("0 0 1 jan,jul *", at(2026, 2, 1, 0, 0)), at(2026, 7, 1, 0, 0));
    // 7 is Sunday as well as 0.
    assert_eq!(next("0 0 * * 7", at(2026, 3, 10, 0, 0)), next("0 0 * * 0", at(2026, 3, 10, 0, 0)));
    assert_eq!(next("0 0 * * 0", at(2026, 3, 10, 0, 0)), at(2026, 3, 15, 0, 0));
}

#[test]
fn shortcuts_and_awkward_days() {
    assert_eq!(next("@daily", at(2026, 3, 10, 12, 0)), at(2026, 3, 11, 0, 0));
    assert_eq!(next("@hourly", at(2026, 3, 10, 12, 30)), at(2026, 3, 10, 13, 0));
    assert_eq!(next("@weekly", at(2026, 3, 10, 12, 0)), at(2026, 3, 15, 0, 0));
    assert_eq!(next("@monthly", at(2026, 3, 10, 12, 0)), at(2026, 4, 1, 0, 0));
    // The 31st skips short months; Feb 29 waits for a leap year.
    assert_eq!(next("0 0 31 * *", at(2026, 4, 1, 0, 0)), at(2026, 5, 31, 0, 0));
    assert_eq!(next("0 12 29 2 *", at(2026, 3, 1, 0, 0)), at(2028, 2, 29, 12, 0));
    // Day-of-month and day-of-week both set: either one matches (standard cron).
    assert_eq!(next("0 0 13 * fri", at(2026, 3, 1, 0, 0)), at(2026, 3, 6, 0, 0));
}

#[test]
fn bad_expressions_say_what_is_wrong() {
    for (bad, word) in [
        ("* * *", "five fields"),
        ("60 * * * *", "minute"),
        ("* 24 * * *", "hour"),
        ("* * 0 * *", "day of month"),
        ("* * * 13 *", "month"),
        ("* * * * 8", "day of week"),
        ("*/0 * * * *", "step"),
        ("a * * * *", "minute"),
        ("5-1 * * * *", "minute"),
        ("1,,2 * * * *", "minute"),
        ("", "five fields"),
    ] {
        let e = CronExpr::parse(bad).unwrap_err();
        assert!(e.contains(word), "{bad:?} -> {e}");
    }
}

#[test]
fn next_after_is_in_the_future_and_on_the_local_wall_clock() {
    let now = chrono::Utc::now().timestamp();
    let expr = CronExpr::parse("0 9 * * *").unwrap();
    let n = expr.next_after(now).unwrap();
    assert!(n > now && n - now <= 25 * 3600, "{}", n - now);
    let local = Local.timestamp_opt(n, 0).single().unwrap();
    assert_eq!((local.hour(), local.minute()), (9, 0));
    let runs = preview("0 9 * * *", now, 3).unwrap();
    assert_eq!(runs.len(), 3);
    assert!(runs[0] < runs[1] && runs[1] < runs[2]);
    assert!(preview("nonsense", now, 3).is_err());
}
