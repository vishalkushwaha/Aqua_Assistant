//! Turning the mouse wheel makes a burst of events. This decides when the window is looked at: while the burst goes on, at
//! most once every `interval_ms` (if "live" is on), and once more when the wheel has been still for `quiet_ms`. A single
//! turn of the wheel is looked at only once, after it has stopped.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollRun {
    /// The wheel is still turning.
    Live,
    /// The wheel has stopped.
    Settled,
}

#[derive(Debug, Clone)]
pub struct ScrollGate {
    interval_ms: u64,
    quiet_ms: u64,
    live: bool,
    last_event: Option<u64>,
    last_run: u64,
    pending_live: bool,
    pending_final: bool,
}

/// A live look is skipped if the final one is about to happen anyway.
const NEAR_FINAL_MS: u64 = 100;

impl ScrollGate {
    pub fn new(interval_ms: u64, quiet_ms: u64, live: bool) -> Self {
        ScrollGate { interval_ms: interval_ms.max(1), quiet_ms, live, last_event: None, last_run: 0, pending_live: false, pending_final: false }
    }

    pub fn configure(&mut self, interval_ms: u64, live: bool) {
        self.interval_ms = interval_ms.max(1);
        self.live = live;
        if !live {
            self.pending_live = false;
        }
    }

    pub fn event(&mut self, now: u64) {
        if self.last_event.map_or(true, |l| now.saturating_sub(l) >= self.quiet_ms) {
            self.last_run = now; // a new burst: the first live look comes one interval after it began
        }
        self.last_event = Some(now);
        self.pending_final = true;
        self.pending_live = self.live;
    }

    fn live_deadline(&self) -> Option<u64> {
        if !self.pending_live {
            return None;
        }
        let d = self.last_run + self.interval_ms;
        (d + NEAR_FINAL_MS < self.last_event? + self.quiet_ms).then_some(d)
    }

    /// When the next look is due, if one is waiting.
    pub fn deadline(&self) -> Option<u64> {
        let fin = if self.pending_final { self.last_event.map(|l| l + self.quiet_ms) } else { None };
        [fin, self.live_deadline()].into_iter().flatten().min()
    }

    pub fn take_due(&mut self, now: u64) -> Option<ScrollRun> {
        if self.pending_final && self.last_event.is_some_and(|l| now >= l + self.quiet_ms) {
            self.pending_final = false;
            self.pending_live = false;
            self.last_run = now;
            return Some(ScrollRun::Settled);
        }
        if self.live_deadline().is_some_and(|d| now >= d) {
            self.pending_live = false;
            self.last_run = now;
            return Some(ScrollRun::Live);
        }
        None
    }

    pub fn cancel(&mut self) {
        self.pending_live = false;
        self.pending_final = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step a virtual clock in 10 ms steps, feeding wheel events at `event_times`, and list what ran and when.
    fn run(gate: &mut ScrollGate, event_times: &[u64], until: u64) -> Vec<(u64, ScrollRun)> {
        let mut out = Vec::new();
        for t in 0..=until {
            if event_times.contains(&t) {
                gate.event(t);
            }
            if t % 10 == 0 {
                while let Some(r) = gate.take_due(t) {
                    out.push((t, r));
                }
            }
        }
        out
    }

    #[test]
    fn one_turn_of_the_wheel_is_looked_at_once_after_it_stops() {
        let mut g = ScrollGate::new(350, 400, true);
        let r = run(&mut g, &[1000], 3000);
        assert_eq!(r, vec![(1400, ScrollRun::Settled)]);
    }

    #[test]
    fn a_long_scroll_is_looked_at_every_interval_and_once_more_when_it_stops() {
        let mut g = ScrollGate::new(350, 400, true);
        let events: Vec<u64> = (0..=2000).step_by(50).collect();
        let r = run(&mut g, &events, 4000);
        let live = r.iter().filter(|(_, k)| *k == ScrollRun::Live).count();
        assert!((5..=6).contains(&live), "{r:?}");
        assert_eq!(r.iter().filter(|(_, k)| *k == ScrollRun::Settled).count(), 1);
        assert_eq!(r.last().unwrap(), &(2400, ScrollRun::Settled));
        // Live looks are never closer together than the interval (the final look may come sooner after the last one).
        let lives: Vec<u64> = r.iter().filter(|(_, k)| *k == ScrollRun::Live).map(|(t, _)| *t).collect();
        for w in lives.windows(2) {
            assert!(w[1] - w[0] >= 340, "{r:?}");
        }
    }

    #[test]
    fn with_live_off_only_the_final_look_happens() {
        let mut g = ScrollGate::new(350, 400, false);
        let events: Vec<u64> = (0..=2000).step_by(50).collect();
        assert_eq!(run(&mut g, &events, 4000), vec![(2400, ScrollRun::Settled)]);
    }

    #[test]
    fn a_new_burst_after_a_pause_starts_afresh() {
        let mut g = ScrollGate::new(350, 400, true);
        let r = run(&mut g, &[0, 3000], 5000);
        assert_eq!(r, vec![(400, ScrollRun::Settled), (3400, ScrollRun::Settled)]);
    }

    #[test]
    fn the_deadline_is_always_in_the_future_of_the_last_look_and_cleared_when_nothing_waits() {
        let mut g = ScrollGate::new(350, 400, true);
        assert_eq!(g.deadline(), None);
        g.event(100);
        assert_eq!(g.deadline(), Some(500));
        g.event(300);
        g.event(450);
        assert_eq!(g.deadline(), Some(450), "the burst has gone on for a full interval: a live look is due, before the final one at 850");
        g.cancel();
        assert_eq!(g.deadline(), None);
    }
}
