//! When a model call is asked again, and after how long (card OA9-02, task PR-03 of the OpenCode audit).
//!
//! The rules are few and written down here so that a provider that answers over a network (a service on another machine, an Ollama that is overloaded) is treated the same way
//! wherever it is called from:
//!
//! * Only a service that said "later" is asked again: [`ProviderError::RateLimited`] and [`ProviderError::Overloaded`]. A refusal, a prompt that does not fit, a bad key, a stopped turn
//!   and a call that gave the model up are not (asking the same thing again gets the same answer). The local server never says "later": a server that cannot be used keeps what the engine
//!   does for it today (one more try on the processor, then `llama-cli`), and does not come here.
//! * **Never after the first output.** Once a piece of the answer has been handed over, a second try would say it again; the router checks that (`router.rs`), this file only counts.
//! * The wait is what the service asked for (`Retry-After`, read by [`retry_after_from_headers`]) when it asked, and when it asked for more than the cap the call is not asked again at
//!   all: a person is not kept waiting a minute for a service that said "in an hour". Without a request from the service the wait grows by doubling, from a base, with a spread of
//!   plus or minus a fifth so that many callers do not come back together, and never passes the cap.
//! * At most [`RetryPolicy::max_retries`] asks again, and never past the time the call was given (`CallControl::deadline`).

use super::error::ProviderError;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How a call that failed with "later" is asked again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    /// The most times one call is asked again (0: never).
    pub max_retries: u32,
    /// The wait before the first ask again when the service gave none; each later one doubles it.
    pub base: Duration,
    /// The longest a call waits before an ask again; a `Retry-After` above it ends the call instead.
    pub cap: Duration,
    /// Spread each wait over 0.8 to 1.2 times itself (a wait the service asked for is kept exactly).
    pub jitter: bool,
}

impl RetryPolicy {
    /// Never asked again.
    pub const NONE: RetryPolicy = RetryPolicy { max_retries: 0, base: Duration::ZERO, cap: Duration::ZERO, jitter: false };

    /// What a router uses unless it is told otherwise: two more tries, after about half a second and then about a second, and a ceiling of ten seconds.
    pub const STANDARD: RetryPolicy = RetryPolicy { max_retries: 2, base: Duration::from_millis(500), cap: Duration::from_secs(10), jitter: true };

    /// The same policy with these limits and no spread (tests, and a caller that wants exact waits).
    pub const fn exact(max_retries: u32, base: Duration, cap: Duration) -> RetryPolicy {
        RetryPolicy { max_retries, base, cap, jitter: false }
    }

    /// Is this the kind of error that is worth asking again at all?
    pub fn applies_to(error: &ProviderError) -> bool {
        matches!(error, ProviderError::RateLimited { .. } | ProviderError::Overloaded { .. })
    }

    /// The wait before ask again number `attempt + 1` (`attempt` is 0 for the first ask again), or `None` when the call is not asked again: an error that is not "later", the asks
    /// used up, or a wait the service asked for that is above the cap.
    pub fn delay(&self, error: &ProviderError, attempt: u32) -> Option<Duration> {
        self.delay_with(error, attempt, if self.jitter { jitter_unit() } else { 0.5 })
    }

    /// [`Self::delay`] with the spread chosen by the caller: `unit` in `[0, 1)` is where in 0.8 to 1.2 times the wait this one falls (0.5 is exactly the wait).
    pub fn delay_with(&self, error: &ProviderError, attempt: u32, unit: f64) -> Option<Duration> {
        if !Self::applies_to(error) || attempt >= self.max_retries {
            return None;
        }
        if let Some(asked) = error.retry_after() {
            // What the service said, exactly; more than the cap is a service that will not be back soon enough to be waited for.
            return (asked <= self.cap).then_some(asked);
        }
        let doubled = self.base.saturating_mul(1u32.checked_shl(attempt.min(30)).unwrap_or(u32::MAX));
        let factor = if self.jitter { 0.8 + 0.4 * unit.clamp(0.0, 1.0) } else { 1.0 };
        let spread = Duration::try_from_secs_f64(doubled.as_secs_f64() * factor).unwrap_or(doubled);
        Some(spread.min(self.cap))
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy::STANDARD
    }
}

/// A number in `[0, 1)` from the operating system's random source (0.5, the middle, when it has none).
fn jitter_unit() -> f64 {
    let mut b = [0u8; 4];
    if getrandom::getrandom(&mut b).is_err() {
        return 0.5;
    }
    f64::from(u32::from_le_bytes(b)) / (f64::from(u32::MAX) + 1.0)
}

/// How long a service asked to wait, from the headers of its reply, as OpenCode reads them (`executor.ts`): `retry-after-ms` first (milliseconds, a number), then `retry-after` as a
/// number of seconds, then `retry-after` as an HTTP date (the wait is the time from `now` to that date, and nothing if it is past). Header names are compared without regard to case.
/// A value that is not a number or a date, or is negative, is no answer, and the next kind is tried.
pub fn retry_after_from_headers<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>, now: SystemTime) -> Option<Duration> {
    let (mut ms, mut after) = (None, None);
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("retry-after-ms") {
            ms = ms.or(Some(value));
        } else if name.eq_ignore_ascii_case("retry-after") {
            after = after.or(Some(value));
        }
    }
    ms.and_then(|v| seconds_from(v, 1000.0)).or_else(|| after.and_then(|v| seconds_from(v, 1.0).or_else(|| until_date(v, now))))
}

/// `value` as a non-negative number of `per_second`ths of a second.
fn seconds_from(value: &str, per_second: f64) -> Option<Duration> {
    let number: f64 = value.trim().parse().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(number / per_second).ok()
}

/// The time from `now` to an HTTP date in any of the three forms HTTP allows (`Sun, 06 Nov 1994 08:49:37 GMT`, `Sunday, 06-Nov-94 08:49:37 GMT`, `Sun Nov  6 08:49:37 1994`).
fn until_date(value: &str, now: SystemTime) -> Option<Duration> {
    use chrono::{DateTime, NaiveDateTime};
    let text = value.trim();
    let at = DateTime::parse_from_rfc2822(text)
        .map(|d| d.timestamp())
        .or_else(|_| NaiveDateTime::parse_from_str(text.trim_end_matches(" GMT"), "%A, %d-%b-%y %H:%M:%S").map(|d| d.and_utc().timestamp()))
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%a %b %e %H:%M:%S %Y").map(|d| d.and_utc().timestamp()))
        .ok()?;
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(Duration::from_secs(at.saturating_sub(now).max(0) as u64))
}

/// Waits `total`, in steps of 50 ms so that the person's Stop, or the caller's own flag, ends the wait at once. `false` when it was stopped. In a test a thread can ask for the waits to be
/// noted instead of waited ([`record_waits`]).
pub(crate) fn wait(total: Duration, stopped: &dyn Fn() -> bool) -> bool {
    #[cfg(test)]
    if tests_support::note(total) {
        return !stopped();
    }
    let started = std::time::Instant::now();
    while let Some(left) = total.checked_sub(started.elapsed()).filter(|l| !l.is_zero()) {
        if stopped() {
            return false;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
    !stopped()
}

#[cfg(test)]
pub(crate) use tests_support::record_waits;

#[cfg(test)]
mod tests_support {
    use std::cell::RefCell;
    use std::time::Duration;

    thread_local! {
        static WAITS: RefCell<Option<Vec<Duration>>> = const { RefCell::new(None) };
    }

    /// Notes a wait when this thread is recording and says so; `false` when it is not (the wait is then really waited).
    pub(super) fn note(total: Duration) -> bool {
        WAITS.with(|w| match w.borrow_mut().as_mut() {
            Some(list) => {
                list.push(total);
                true
            }
            None => false,
        })
    }

    /// Runs `f` with the waits of the retry policy noted and not waited, and gives them back, in order, with what `f` returned.
    pub(crate) fn record_waits<R>(f: impl FnOnce() -> R) -> (R, Vec<Duration>) {
        let before = WAITS.with(|w| w.replace(Some(Vec::new())));
        let out = f();
        let waits = WAITS.with(|w| w.replace(before)).unwrap_or_default();
        (out, waits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn later(after: Option<u64>) -> ProviderError {
        ProviderError::RateLimited { retry_after: after.map(Duration::from_secs) }
    }

    fn at(secs_since_epoch: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs_since_epoch)
    }

    #[test]
    fn only_a_service_that_said_later_is_asked_again_and_only_so_many_times() {
        let policy = RetryPolicy::exact(2, Duration::from_millis(500), Duration::from_secs(10));
        for error in [later(None), ProviderError::Overloaded { retry_after: None }] {
            assert_eq!(policy.delay(&error, 0), Some(Duration::from_millis(500)), "{}", error.class());
            assert_eq!(policy.delay(&error, 1), Some(Duration::from_secs(1)), "doubled");
            assert_eq!(policy.delay(&error, 2), None, "two asks again and no more");
        }
        for error in [
            ProviderError::Transport("reset".into()),
            ProviderError::Unavailable { detail: "down".into() },
            ProviderError::ContextOverflow { limit: Some(2048), detail: "too long".into() },
            ProviderError::Rejected("no".into()),
            ProviderError::Auth("key".into()),
            ProviderError::Quota("cap".into()),
            ProviderError::ContentPolicy("no".into()),
            ProviderError::Timeout { idle: true, detail: "quiet".into() },
            ProviderError::Cancelled,
            ProviderError::Yielded,
            ProviderError::InvalidOutput("?".into()),
        ] {
            assert_eq!(policy.delay(&error, 0), None, "{} is not a reason to ask again", error.class());
        }
        assert_eq!(RetryPolicy::NONE.delay(&later(None), 0), None, "a policy of none never asks again");
        assert_eq!(RetryPolicy::default(), RetryPolicy::STANDARD);
    }

    #[test]
    fn a_wait_the_service_asked_for_is_kept_exactly_and_one_above_the_cap_ends_the_call() {
        let policy = RetryPolicy { jitter: true, ..RetryPolicy::STANDARD };
        // Retry-After: 3 is three seconds, whatever the spread says and however far into the asks it is.
        for unit in [0.0, 0.5, 0.99] {
            assert_eq!(policy.delay_with(&later(Some(3)), 0, unit), Some(Duration::from_secs(3)), "unit {unit}");
        }
        assert_eq!(policy.delay_with(&later(Some(10)), 1, 0.3), Some(Duration::from_secs(10)), "up to the cap is waited");
        assert_eq!(policy.delay_with(&later(Some(11)), 0, 0.3), None, "past the cap the service is not waited for");
        assert_eq!(policy.delay_with(&later(Some(3_600)), 0, 0.3), None, "an hour is not waited for");
        assert_eq!(policy.delay_with(&ProviderError::Overloaded { retry_after: Some(Duration::ZERO) }, 0, 0.3), Some(Duration::ZERO), "a service that says now is asked now");
    }

    #[test]
    fn without_a_request_from_the_service_the_wait_doubles_with_a_fifth_either_way_and_stops_at_the_cap() {
        let policy = RetryPolicy { max_retries: 8, base: Duration::from_secs(1), cap: Duration::from_secs(10), jitter: true };
        let error = ProviderError::Overloaded { retry_after: None };
        // The spread: 0.8 at the bottom of the range, 1.0 in the middle, 1.2 at the top (not reached).
        assert_eq!(policy.delay_with(&error, 0, 0.0), Some(Duration::from_millis(800)));
        assert_eq!(policy.delay_with(&error, 0, 0.5), Some(Duration::from_secs(1)));
        assert_eq!(policy.delay_with(&error, 1, 0.5), Some(Duration::from_secs(2)));
        let high = policy.delay_with(&error, 0, 0.999_999).unwrap();
        assert!(high > Duration::from_millis(1199) && high <= Duration::from_millis(1200), "{high:?}");
        // The fourth ask again is eight seconds, give or take a fifth; the ones after it are at the cap, and a huge attempt number cannot overflow.
        assert_eq!(policy.delay_with(&error, 3, 0.5), Some(Duration::from_secs(8)));
        assert!(policy.delay_with(&error, 3, 0.99).unwrap() < Duration::from_secs(10));
        for attempt in [4, 5, 7] {
            assert_eq!(policy.delay_with(&error, attempt, 0.0), Some(Duration::from_secs(10)), "attempt {attempt}: 16 s and more, at the cap");
        }
        let long = RetryPolicy { max_retries: u32::MAX, ..policy };
        assert_eq!(long.delay_with(&error, u32::MAX - 1, 0.5), Some(Duration::from_secs(10)));
        // The real spread stays in range, whatever the random source says.
        for _ in 0..50 {
            let d = policy.delay(&error, 0).unwrap();
            assert!(d >= Duration::from_millis(800) && d <= Duration::from_millis(1200), "{d:?}");
        }
        // Without the spread the wait is the doubled base.
        let exact = RetryPolicy::exact(3, Duration::from_millis(250), Duration::from_secs(10));
        assert_eq!((exact.delay(&error, 0), exact.delay(&error, 1), exact.delay(&error, 2)), (Some(Duration::from_millis(250)), Some(Duration::from_millis(500)), Some(Duration::from_secs(1))));
    }

    #[test]
    fn retry_after_is_read_in_milliseconds_then_in_seconds_then_as_a_date() {
        let now = at(784_111_777); // Sun, 06 Nov 1994 08:49:37 GMT
        let read = |headers: &[(&str, &str)]| retry_after_from_headers(headers.iter().copied(), now);
        // retry-after-ms wins over retry-after, whatever the order and the case of the names.
        assert_eq!(read(&[("Retry-After", "30"), ("retry-after-ms", "1500")]), Some(Duration::from_millis(1500)));
        assert_eq!(read(&[("RETRY-AFTER-MS", "250")]), Some(Duration::from_millis(250)));
        assert_eq!(read(&[("Retry-After", "3")]), Some(Duration::from_secs(3)));
        assert_eq!(read(&[("retry-after", " 120 ")]), Some(Duration::from_secs(120)), "spaces around the number");
        assert_eq!(read(&[("retry-after", "0.5")]), Some(Duration::from_millis(500)), "a fraction is read too");
        // The three forms of an HTTP date, 42 seconds from now.
        for date in ["Sun, 06 Nov 1994 08:50:19 GMT", "Sunday, 06-Nov-94 08:50:19 GMT", "Sun Nov  6 08:50:19 1994"] {
            assert_eq!(read(&[("Retry-After", date)]), Some(Duration::from_secs(42)), "{date}");
        }
        assert_eq!(read(&[("Retry-After", "Sun, 06 Nov 1994 08:00:00 GMT")]), Some(Duration::ZERO), "a date in the past is no wait");
        // What is not an answer: nothing, a word, a negative number, an infinite one, a bad date; a bad -ms falls through to the seconds.
        let none: [&[(&str, &str)]; 7] = [&[], &[("retry-after", "soon")], &[("retry-after", "-5")], &[("retry-after", "inf")], &[("retry-after", "NaN")], &[("retry-after", "Sun, 99 Nov 1994 08:50:19 GMT")], &[("content-type", "text/plain")]];
        for headers in none {
            assert_eq!(read(headers), None, "{headers:?}");
        }
        assert_eq!(read(&[("retry-after-ms", "-1"), ("retry-after", "7")]), Some(Duration::from_secs(7)));
        assert_eq!(read(&[("retry-after-ms", "many"), ("retry-after", "7")]), Some(Duration::from_secs(7)));
        // A number too large for a duration does not panic.
        assert_eq!(read(&[("retry-after", "1e300")]), None);
        // The first of two headers of the same name is the one read.
        assert_eq!(read(&[("retry-after", "2"), ("retry-after", "9")]), Some(Duration::from_secs(2)));
    }

    #[test]
    fn a_wait_is_ended_by_a_stop_and_a_test_can_note_its_waits_without_waiting() {
        let started = std::time::Instant::now();
        assert!(wait(Duration::from_millis(120), &|| false));
        assert!(started.elapsed() >= Duration::from_millis(120));
        let started = std::time::Instant::now();
        assert!(!wait(Duration::from_secs(30), &|| true), "a Stop ends the wait at once");
        assert!(started.elapsed() < Duration::from_secs(2));
        let (done, waits) = record_waits(|| wait(Duration::from_secs(3), &|| false));
        assert!(done);
        assert_eq!(waits, vec![Duration::from_secs(3)], "noted, and not waited");
        let (done, _) = record_waits(|| wait(Duration::from_secs(3), &|| true));
        assert!(!done, "a Stop is seen when the waits are only noted too");
    }
}
