//! Several events in a burst (a tab switch fires a focus change, a title change and a size change) become one check.

#[derive(Debug, Clone)]
pub struct Debouncer {
    quiet_ms: u64,
    max_ms: u64,
    first: Option<u64>,
    last: Option<u64>,
}

impl Debouncer {
    /// Fire when nothing new has arrived for `quiet_ms`, or at the latest `max_ms` after the first event of the burst.
    pub fn new(quiet_ms: u64, max_ms: u64) -> Self {
        Debouncer { quiet_ms, max_ms: max_ms.max(quiet_ms), first: None, last: None }
    }

    pub fn push(&mut self, now: u64) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    pub fn pending(&self) -> bool {
        self.first.is_some()
    }

    /// When the check should happen, if one is waiting.
    pub fn deadline(&self) -> Option<u64> {
        let (f, l) = (self.first?, self.last?);
        Some((l + self.quiet_ms).min(f + self.max_ms))
    }

    /// True once, when the burst is over; the burst is then forgotten.
    pub fn take_due(&mut self, now: u64) -> bool {
        match self.deadline() {
            Some(d) if now >= d => {
                self.first = None;
                self.last = None;
                true
            }
            _ => false,
        }
    }

    pub fn clear(&mut self) {
        self.first = None;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_becomes_one_check_after_it_goes_quiet() {
        let mut d = Debouncer::new(250, 1000);
        assert!(!d.pending() && d.deadline().is_none());
        d.push(0);
        d.push(100);
        d.push(300);
        assert!(!d.take_due(500), "events were still arriving 200 ms ago");
        assert!(d.take_due(550));
        assert!(!d.take_due(10_000), "fires once");
        assert!(!d.pending());
    }

    #[test]
    fn a_constant_stream_cannot_postpone_the_check_for_ever() {
        let mut d = Debouncer::new(250, 1000);
        for t in (0..2000).step_by(100) {
            d.push(t);
            if d.take_due(t) {
                assert!(t >= 1000);
                return;
            }
        }
        panic!("never fired");
    }
}
