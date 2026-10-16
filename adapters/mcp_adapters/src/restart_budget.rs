//! How often a server of the person's may be started again (card OA11-03, task EX-10).
//!
//! A server that is trusted and then crashes is started again by the next call that needs it (a call that timed out ends it too). A server that crashes at once, every time, would be
//! started again by every call of every run, each start a new process and a new listing of its tools. The budget stops that: at most [`MAX_RESTARTS`] starts again in any
//! [`WINDOW`], the first at once and the next ones after a wait ([`BACKOFF`]), and the one after is refused with a reason. The server is then `Failed` (the person sees why in the list
//! of servers) until the person trusts it again, which starts it from a new budget.
//!
//! The first start of a server is not counted: it is the person's act (trusting it) or Aqua's start-up.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How many times a stopped server is started again within [`WINDOW`].
pub const MAX_RESTARTS: usize = 3;
/// The time in which [`MAX_RESTARTS`] are allowed (ten minutes).
pub const WINDOW: Duration = Duration::from_secs(600);
/// The wait before the first, second and third start again: none, one second, four seconds. (The first is at once: a server that ended once is most often a server that was ended on
/// purpose, by a call that ran out of time.)
pub const BACKOFF: [Duration; MAX_RESTARTS] = [Duration::ZERO, Duration::from_secs(1), Duration::from_secs(4)];

/// The restarts of one server and when they were.
#[derive(Debug, Clone)]
pub struct RestartBudget {
    max: usize,
    window: Duration,
    backoff: Vec<Duration>,
    times: VecDeque<Instant>,
}

impl Default for RestartBudget {
    fn default() -> Self {
        Self { max: MAX_RESTARTS, window: WINDOW, backoff: BACKOFF.to_vec(), times: VecDeque::new() }
    }
}

impl RestartBudget {
    /// A budget with its own limits (the tests use short ones; the program uses [`Default`]).
    pub fn with_limits(max: usize, window: Duration, backoff: Vec<Duration>) -> Self {
        Self { max, window, backoff, times: VecDeque::new() }
    }

    /// Asks to start the server again at `now`. `Ok(wait)` is how long to wait before starting it (and the restart is counted); `Err` says why it may not be started again.
    pub fn permit(&mut self, now: Instant) -> Result<Duration, String> {
        while self.times.front().is_some_and(|t| now.saturating_duration_since(*t) >= self.window) {
            self.times.pop_front();
        }
        if self.times.len() >= self.max {
            return Err(format!("it was started again {} times in {} minutes and ended each time: it is not started again until you trust it again", self.max, (self.window.as_secs() / 60).max(1)));
        }
        let wait = self.backoff.get(self.times.len()).or(self.backoff.last()).copied().unwrap_or(Duration::ZERO);
        self.times.push_back(now);
        Ok(wait)
    }

    /// How many restarts are counted at `now`.
    pub fn used(&self, now: Instant) -> usize {
        self.times.iter().filter(|t| now.saturating_duration_since(**t) < self.window).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_restarts_in_ten_minutes_are_allowed_with_a_longer_wait_each_time_and_the_fourth_is_refused() {
        let mut b = RestartBudget::default();
        let t0 = Instant::now();
        assert_eq!(b.permit(t0), Ok(Duration::ZERO));
        assert_eq!(b.permit(t0 + Duration::from_secs(5)), Ok(Duration::from_secs(1)));
        assert_eq!(b.permit(t0 + Duration::from_secs(20)), Ok(Duration::from_secs(4)));
        let refused = b.permit(t0 + Duration::from_secs(30)).unwrap_err();
        assert!(refused.contains("3 times in 10 minutes") && refused.contains("trust it again"), "{refused}");
        // Refusing does not count: it is still refused, and the count does not grow.
        assert!(b.permit(t0 + Duration::from_secs(31)).is_err());
        assert_eq!(b.used(t0 + Duration::from_secs(31)), 3);
    }

    #[test]
    fn restarts_older_than_the_window_are_forgotten() {
        let mut b = RestartBudget::default();
        let t0 = Instant::now();
        for i in 0..3 {
            b.permit(t0 + Duration::from_secs(i)).unwrap();
        }
        assert!(b.permit(t0 + Duration::from_secs(599)).is_err(), "still inside the window");
        // Ten minutes after the first one it is out of the window; the other two still count, so there is one place.
        assert_eq!(b.permit(t0 + Duration::from_secs(600)), Ok(Duration::from_secs(4)), "two are in the window: the third wait");
        assert!(b.permit(t0 + Duration::from_secs(600)).is_err(), "and now it is full again");
        assert_eq!(b.used(t0 + Duration::from_secs(5000)), 0);
    }

    #[test]
    fn a_budget_with_its_own_limits_is_held_to_them() {
        let mut b = RestartBudget::with_limits(1, Duration::from_secs(60), vec![Duration::ZERO]);
        let t0 = Instant::now();
        assert!(b.permit(t0).is_ok());
        assert!(b.permit(t0).is_err());
        let none = RestartBudget::with_limits(0, Duration::from_secs(60), vec![]);
        assert!(none.clone().permit(t0).is_err(), "a budget of none allows none");
    }
}
