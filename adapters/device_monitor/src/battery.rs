//! The battery state machine for one device: when to warn, how a warning is confirmed, when it resets.
//! Pure and clock-free (the caller passes the time), so every case is tested.

use super::model::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Ok = 0,
    Low = 1,
    VeryLow = 2,
    Critical = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub low: u8,
    pub very_low: u8,
    pub critical: u8,
}

impl Thresholds {
    pub fn level_of(&self, percent: u8) -> Level {
        if percent <= self.critical {
            Level::Critical
        } else if percent <= self.very_low {
            Level::VeryLow
        } else if percent <= self.low {
            Level::Low
        } else {
            Level::Ok
        }
    }
    pub fn threshold_of(&self, level: Level) -> u8 {
        match level {
            Level::Ok => 100,
            Level::Low => self.low,
            Level::VeryLow => self.very_low,
            Level::Critical => self.critical,
        }
    }
}

/// How far above a threshold the battery has to rise before that warning can be given again.
pub const RESET_MARGIN: u8 = 5;
/// A low (not critical) reading is confirmed by a second reading at least this long after the first.
pub const CONFIRM_MS: u64 = 2_500;
/// How long after the first low reading the device is read again.
pub const REREAD_MS: u64 = 3_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Read the device again after this many milliseconds to confirm a low reading.
    Reread { after_ms: u64 },
    /// Cross a threshold downward: warn about `level`.
    Warn { level: Level, percent: u8 },
    /// The battery rose well above the warning level (or charging started).
    Recovered { percent: u8 },
    ChargingStarted,
    FullyCharged,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BatteryMachine {
    /// The worst level already warned about during this connection.
    warned: Level,
    /// A low reading waiting for its confirmation: (level, first seen at).
    pending: Option<(Level, u64)>,
    last_percent: Option<u8>,
    last_charging: Charging,
    charging_announced: Option<Charging>,
}

impl Default for Level {
    fn default() -> Self {
        Level::Ok
    }
}

impl BatteryMachine {
    /// A new connection: everything is forgotten. (A headset charged while off and reconnected at 25 percent must warn
    /// again when it falls.)
    pub fn reset(&mut self) {
        *self = BatteryMachine::default();
    }

    pub fn warned(&self) -> Level {
        self.warned
    }

    pub fn last_percent(&self) -> Option<u8> {
        self.last_percent
    }

    pub fn last_charging(&self) -> Charging {
        self.last_charging
    }

    /// Mark a level as already announced (for example inside the "connected" sentence).
    pub fn mark_warned(&mut self, level: Level) {
        if level > self.warned {
            self.warned = level;
        }
        self.pending = None;
    }

    /// Feed one reading. `immediate`: skip the confirmation (the first reading after a connection, which is itself a fresh read).
    pub fn reading(&mut self, r: &BatteryReading, now: u64, th: &Thresholds, infer_charging: bool, immediate: bool) -> Vec<Outcome> {
        let mut out = Vec::new();

        // Charging state: from the source, or (only if the user asked for the guess) from a rising level.
        let mut charging = r.charging;
        if charging == Charging::Unknown && infer_charging {
            if let (Some(prev), Some(p)) = (self.last_percent, r.percent) {
                if p >= prev.saturating_add(3) {
                    charging = Charging::Charging;
                } else if p < prev {
                    charging = Charging::Discharging;
                } else {
                    charging = self.last_charging;
                }
            }
        }
        let reliable = r.source != BatterySource::None;
        if reliable && charging != self.last_charging {
            match charging {
                Charging::Charging if self.charging_announced != Some(Charging::Charging) => {
                    out.push(Outcome::ChargingStarted);
                    self.charging_announced = Some(Charging::Charging);
                }
                Charging::Full if self.charging_announced != Some(Charging::Full) => {
                    out.push(Outcome::FullyCharged);
                    self.charging_announced = Some(Charging::Full);
                }
                Charging::Discharging | Charging::NotCharging => {
                    // unplugged: the next charge may be announced again
                    self.charging_announced = None;
                }
                _ => {}
            }
        }
        self.last_charging = charging;

        let Some(p) = r.percent else { return out };
        self.last_percent = Some(p);
        let lvl = th.level_of(p);

        // Reset: the battery has risen clearly above the level that was warned about, or it is charging.
        if self.warned != Level::Ok && (p >= th.threshold_of(self.warned).saturating_add(RESET_MARGIN) || charging == Charging::Charging) {
            let was = self.warned;
            // The warned level drops to the level of the battery now (never worse than it was): a level above the
            // current one can warn again if the battery falls back into it.
            self.warned = lvl.min(self.warned);
            if self.warned == Level::Ok && was != Level::Ok {
                out.push(Outcome::Recovered { percent: p });
            }
            self.pending = None;
        }

        // While it is charging (known), nothing is warned.
        if charging == Charging::Charging {
            self.pending = None;
            return out;
        }

        if lvl > self.warned {
            if lvl == Level::Critical || immediate {
                self.warned = lvl;
                self.pending = None;
                out.push(Outcome::Warn { level: lvl, percent: p });
            } else {
                match self.pending {
                    Some((_, since)) if now.saturating_sub(since) >= CONFIRM_MS => {
                        self.warned = lvl;
                        self.pending = None;
                        out.push(Outcome::Warn { level: lvl, percent: p });
                    }
                    Some((_, since)) => {
                        // too soon to confirm: ask again when it will be time
                        let waited = now.saturating_sub(since);
                        self.pending = Some((lvl, since));
                        out.push(Outcome::Reread { after_ms: CONFIRM_MS.saturating_sub(waited).max(500) });
                    }
                    None => {
                        self.pending = Some((lvl, now));
                        out.push(Outcome::Reread { after_ms: REREAD_MS });
                    }
                }
            }
        } else {
            self.pending = None;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TH: Thresholds = Thresholds { low: 20, very_low: 10, critical: 5 };

    fn r(p: u8) -> BatteryReading {
        BatteryReading { percent: Some(p), charging: Charging::Unknown, source: BatterySource::WindowsProperty }
    }
    fn rc(p: u8, c: Charging) -> BatteryReading {
        BatteryReading { percent: Some(p), charging: c, source: BatterySource::WindowsProperty }
    }
    fn warns(o: &[Outcome]) -> Vec<(Level, u8)> {
        o.iter().filter_map(|x| if let Outcome::Warn { level, percent } = x { Some((*level, *percent)) } else { None }).collect()
    }

    #[test]
    fn levels_follow_the_thresholds() {
        assert_eq!(TH.level_of(80), Level::Ok);
        assert_eq!(TH.level_of(21), Level::Ok);
        assert_eq!(TH.level_of(20), Level::Low);
        assert_eq!(TH.level_of(11), Level::Low);
        assert_eq!(TH.level_of(10), Level::VeryLow);
        assert_eq!(TH.level_of(6), Level::VeryLow);
        assert_eq!(TH.level_of(5), Level::Critical);
        assert_eq!(TH.level_of(0), Level::Critical);
    }

    #[test]
    fn a_low_reading_is_confirmed_by_a_second_one_and_warns_once() {
        let mut m = BatteryMachine::default();
        assert!(m.reading(&r(40), 0, &TH, false, false).is_empty());
        let o = m.reading(&r(20), 1_000, &TH, false, false);
        assert_eq!(o, vec![Outcome::Reread { after_ms: REREAD_MS }], "one low reading is not enough");
        // the re-read 3 s later agrees
        let o = m.reading(&r(20), 4_000, &TH, false, false);
        assert_eq!(warns(&o), vec![(Level::Low, 20)]);
        // still low: nothing more is said, however often it is read
        for t in 5..20 {
            assert!(m.reading(&r(19), t * 60_000, &TH, false, false).is_empty());
        }
        assert!(m.reading(&r(18), 30 * 60_000, &TH, false, false).is_empty());
    }

    #[test]
    fn a_single_odd_reading_does_not_warn() {
        let mut m = BatteryMachine::default();
        assert_eq!(m.reading(&r(18), 0, &TH, false, false).len(), 1); // asks for a re-read
        assert!(m.reading(&r(60), 3_000, &TH, false, false).is_empty(), "the re-read disagrees: forgotten");
        // later it really drops: the confirmation starts again
        assert!(matches!(m.reading(&r(19), 600_000, &TH, false, false)[0], Outcome::Reread { .. }));
    }

    #[test]
    fn critical_needs_one_reading_and_going_past_levels_announces_only_the_lowest() {
        let mut m = BatteryMachine::default();
        assert!(m.reading(&r(30), 0, &TH, false, false).is_empty());
        let o = m.reading(&r(4), 1_000, &TH, false, false);
        assert_eq!(warns(&o), vec![(Level::Critical, 4)]);
        // 22 -> 9 between two samples: only "very low", and Low counts as warned too
        let mut m = BatteryMachine::default();
        m.reading(&r(22), 0, &TH, false, false);
        let o = m.reading(&r(9), 60_000, &TH, false, false);
        assert!(matches!(o[0], Outcome::Reread { .. }));
        let o = m.reading(&r(9), 64_000, &TH, false, false);
        assert_eq!(warns(&o), vec![(Level::VeryLow, 9)]);
        // after "very low" a drop to low-range values says nothing about Low again
        let mut m2 = BatteryMachine::default();
        m2.mark_warned(Level::VeryLow);
        assert!(m2.reading(&r(19), 0, &TH, false, false).is_empty());
    }

    #[test]
    fn each_level_warns_once_on_the_way_down() {
        let mut m = BatteryMachine::default();
        let mut said = Vec::new();
        let mut t = 0u64;
        for p in [50u8, 30, 20, 20, 15, 10, 10, 8, 5, 4, 3] {
            t += 60_000;
            let mut o = m.reading(&r(p), t, &TH, false, false);
            if o.iter().any(|x| matches!(x, Outcome::Reread { .. })) {
                t += 3_000;
                o = m.reading(&r(p), t, &TH, false, false);
            }
            said.extend(warns(&o));
        }
        assert_eq!(said, vec![(Level::Low, 20), (Level::VeryLow, 10), (Level::Critical, 5)]);
    }

    #[test]
    fn a_warning_resets_when_the_battery_rises_clearly_and_can_warn_again() {
        let mut m = BatteryMachine::default();
        m.reading(&r(8), 0, &TH, false, true); // immediate: very low
        assert_eq!(m.warned(), Level::VeryLow);
        // 12 is only 2 above the very-low threshold: no reset yet
        m.reading(&r(12), 1_000, &TH, false, false);
        assert_eq!(m.warned(), Level::VeryLow);
        // 16 is 6 above 10: reset to the Low level; it is still low, so nothing new is warned
        m.reading(&r(16), 2_000, &TH, false, false);
        assert_eq!(m.warned(), Level::Low);
        // well above: recovered, and it can warn again later
        let o = m.reading(&r(60), 3_000, &TH, false, false);
        assert!(o.contains(&Outcome::Recovered { percent: 60 }));
        assert_eq!(m.warned(), Level::Ok);
        let o = m.reading(&r(19), 4_000, &TH, false, false);
        assert!(matches!(o[0], Outcome::Reread { .. }));
    }

    #[test]
    fn a_reconnection_forgets_everything() {
        let mut m = BatteryMachine::default();
        m.reading(&r(8), 0, &TH, false, true);
        assert_eq!(m.warned(), Level::VeryLow);
        m.reset();
        assert_eq!(m.warned(), Level::Ok);
        // reconnected at 25 percent, it falls to 19: warns (after confirmation)
        assert!(m.reading(&r(25), 0, &TH, false, true).is_empty());
        assert_eq!(warns(&m.reading(&r(19), 1_000, &TH, false, true)), vec![(Level::Low, 19)]);
    }

    #[test]
    fn nothing_is_warned_while_it_is_known_to_be_charging() {
        let mut m = BatteryMachine::default();
        let o = m.reading(&rc(8, Charging::Charging), 0, &TH, false, false);
        assert!(warns(&o).is_empty() && o.contains(&Outcome::ChargingStarted));
        assert!(m.reading(&rc(9, Charging::Charging), 60_000, &TH, false, false).is_empty());
        // unplugged at 9 percent: it warns, after confirmation
        let o = m.reading(&rc(9, Charging::Discharging), 120_000, &TH, false, false);
        assert!(matches!(o[0], Outcome::Reread { .. }));
    }

    #[test]
    fn charging_and_full_are_reported_once_and_only_from_a_source_that_knows() {
        let mut m = BatteryMachine::default();
        assert!(m.reading(&rc(50, Charging::Unknown), 0, &TH, false, false).is_empty());
        let o = m.reading(&rc(60, Charging::Charging), 1_000, &TH, false, false);
        assert_eq!(o, vec![Outcome::ChargingStarted]);
        assert!(m.reading(&rc(70, Charging::Charging), 2_000, &TH, false, false).is_empty(), "not announced twice");
        let o = m.reading(&rc(100, Charging::Full), 3_000, &TH, false, false);
        assert_eq!(o, vec![Outcome::FullyCharged]);
        // a reading with no source never claims charging
        let mut m = BatteryMachine::default();
        let none = BatteryReading { percent: Some(50), charging: Charging::Charging, source: BatterySource::None };
        assert!(m.reading(&none, 0, &TH, false, false).is_empty());
    }

    #[test]
    fn rising_means_charging_only_when_that_guess_is_switched_on() {
        let mut m = BatteryMachine::default();
        m.reading(&r(40), 0, &TH, true, false);
        assert_eq!(m.reading(&r(46), 60_000, &TH, true, false), vec![Outcome::ChargingStarted]);
        let mut m = BatteryMachine::default();
        m.reading(&r(40), 0, &TH, false, false);
        assert!(m.reading(&r(46), 60_000, &TH, false, false).is_empty());
    }

    #[test]
    fn a_reading_with_no_percentage_changes_nothing() {
        let mut m = BatteryMachine::default();
        let none = BatteryReading { percent: None, charging: Charging::Unknown, source: BatterySource::WindowsProperty };
        assert!(m.reading(&none, 0, &TH, false, false).is_empty());
        assert_eq!(m.last_percent(), None);
    }
}
