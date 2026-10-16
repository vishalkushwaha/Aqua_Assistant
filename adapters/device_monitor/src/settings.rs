//! What the user can change, and what Aqua remembers about devices it has seen. One versioned JSON file, written atomically.

use super::model::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Per-device choices. `None` means "follow the general setting".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DevicePrefs {
    /// Never announce anything about this device.
    pub ignore: bool,
    pub connect: Option<bool>,
    pub disconnect: Option<bool>,
    pub battery: Option<bool>,
    pub charging: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuietHours {
    pub enabled: bool,
    /// Local hour the quiet period starts and ends (it may wrap past midnight).
    pub start_hour: u8,
    pub end_hour: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The master switch.
    pub enabled: bool,
    pub announce_connections: bool,
    /// Off by default: the noisiest announcement.
    pub announce_disconnections: bool,
    pub announce_low_battery: bool,
    pub announce_critical_battery: bool,
    pub announce_charging: bool,
    pub announce_fully_charged: bool,
    pub announce_recovered: bool,
    /// "Unknown USB device connected" is useful as a security hint, so it is on by default.
    pub announce_unknown_usb: bool,
    pub low_percent: u8,
    pub very_low_percent: u8,
    pub critical_percent: u8,
    pub battery_cooldown_secs: u64,
    pub charging_cooldown_secs: u64,
    pub rate_limit_per_minute: u32,
    /// Treat a rising battery level as charging when the device does not say. A guess, so off by default.
    pub infer_charging: bool,
    /// Repeat a critical warning every this many minutes while it stays critical. 0 = never.
    pub repeat_critical_minutes: u32,
    pub quiet_hours: QuietHours,
    pub disabled_categories: BTreeSet<Category>,
    pub devices: BTreeMap<String, DevicePrefs>,
    /// Wording. Missing keys use the defaults (`default_templates`).
    pub templates: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            announce_connections: true,
            announce_disconnections: false,
            announce_low_battery: true,
            announce_critical_battery: true,
            announce_charging: false,
            announce_fully_charged: false,
            announce_recovered: false,
            announce_unknown_usb: true,
            low_percent: 20,
            very_low_percent: 10,
            critical_percent: 5,
            battery_cooldown_secs: 600,
            charging_cooldown_secs: 60,
            rate_limit_per_minute: 6,
            infer_charging: false,
            repeat_critical_minutes: 0,
            quiet_hours: QuietHours { enabled: false, start_hour: 22, end_hour: 7 },
            disabled_categories: BTreeSet::new(),
            devices: BTreeMap::new(),
            templates: BTreeMap::new(),
        }
    }
}

pub fn default_templates() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("connected", "{name} connected."),
        ("connected_many", "{names} connected."),
        ("connected_battery", "{name} connected, battery {pct} percent."),
        ("connected_battery_critical", "{name} connected. Warning. Its battery is critically low."),
        ("disconnected", "{name} disconnected."),
        ("disconnected_dead", "{name} disconnected. It may have run out of battery."),
        ("battery_low", "Your {name} {has} {pct} percent battery remaining."),
        ("battery_critical", "Warning. Your {name} {is} critically low on battery."),
        ("charging", "Your {name} {is} now charging."),
        ("fully_charged", "Your {name} {is} fully charged."),
        ("recovered", "Your {name} {has} recovered to {pct} percent."),
        ("computer_battery_low", "Your computer has {pct} percent battery remaining."),
        ("computer_battery_critical", "Warning. Your computer is critically low on battery."),
        ("computer_charging", "Your computer is now charging."),
        ("computer_fully_charged", "Your computer is fully charged."),
    ])
}

impl Settings {
    /// Reject out-of-order or silly values. The caller keeps the old settings when this fails.
    pub fn validate(&self) -> Result<(), String> {
        let (l, v, c) = (self.low_percent, self.very_low_percent, self.critical_percent);
        if !(1..=99).contains(&c) || !(1..=99).contains(&v) || !(1..=99).contains(&l) {
            return Err("battery levels must be between 1 and 99 percent".into());
        }
        if !(l > v && v > c) {
            return Err(format!("battery levels must go low ({l}) > very low ({v}) > critical ({c})"));
        }
        if self.rate_limit_per_minute == 0 || self.rate_limit_per_minute > 60 {
            return Err("the announcement limit must be between 1 and 60 a minute".into());
        }
        if self.quiet_hours.start_hour > 23 || self.quiet_hours.end_hour > 23 {
            return Err("quiet hours must be hours between 0 and 23".into());
        }
        for (k, t) in &self.templates {
            if t.chars().count() > 200 {
                return Err(format!("the wording for '{k}' is too long (200 characters at most)"));
            }
            if !default_templates().contains_key(k.as_str()) {
                return Err(format!("unknown wording key '{k}'"));
            }
        }
        if self.devices.len() > 1000 {
            return Err("too many per-device settings".into());
        }
        Ok(())
    }

    pub fn template(&self, key: &str) -> String {
        self.templates
            .get(key)
            .cloned()
            .or_else(|| default_templates().get(key).map(|s| s.to_string()))
            .unwrap_or_else(|| "{name}".to_string())
    }

    pub fn prefs(&self, id: &str) -> DevicePrefs {
        self.devices.get(id).cloned().unwrap_or_default()
    }
}

/// A device Aqua has seen, remembered so the panel can list it and keep its settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Known {
    pub name: String,
    pub device_type: DeviceType,
    pub connection: Connection,
    pub id_kind: IdKind,
    /// Unix seconds.
    pub last_seen: u64,
}

pub const STATE_VERSION: u32 = 1;
const RETAIN_SECS: u64 = 180 * 86_400;
const MAX_KNOWN: usize = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    /// Random per installation: device references and log hashes are salted with it.
    pub salt: u64,
    pub settings: Settings,
    pub known: BTreeMap<String, Known>,
}

impl Default for State {
    fn default() -> Self {
        Self { version: STATE_VERSION, salt: 0, settings: Settings::default(), known: BTreeMap::new() }
    }
}

impl State {
    /// Load the file. A missing file is a fresh start; a damaged one is moved aside (kept for inspection) and replaced by
    /// the defaults; a file from a newer Aqua is left alone and the defaults are used for this run.
    pub fn load(path: &Path) -> (State, Option<String>) {
        let Ok(text) = std::fs::read_to_string(path) else { return (State::default(), None) };
        match serde_json::from_str::<State>(&text) {
            Ok(mut s) if s.version <= STATE_VERSION => {
                if let Err(e) = s.settings.validate() {
                    return (State { known: s.known, ..State::default() }, Some(format!("saved settings were invalid ({e}); defaults are used")));
                }
                s.version = STATE_VERSION;
                (s, None)
            }
            Ok(s) => (State::default(), Some(format!("the saved file is from a newer Aqua (version {}); it is left alone", s.version))),
            Err(e) => {
                let aside = path.with_extension("json.damaged");
                let _ = std::fs::rename(path, &aside);
                (State::default(), Some(format!("the saved file was damaged ({e}); it was moved aside and defaults are used")))
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Drop devices not seen for 180 days and keep at most 500 (never one the user has set preferences for).
    pub fn prune(&mut self, now_secs: u64) {
        let keep = |id: &str, k: &Known, s: &Settings| s.devices.contains_key(id) || now_secs.saturating_sub(k.last_seen) < RETAIN_SECS;
        let settings = self.settings.clone();
        self.known.retain(|id, k| keep(id, k, &settings));
        if self.known.len() > MAX_KNOWN {
            let mut by_age: Vec<(String, u64)> = self.known.iter().filter(|(id, _)| !settings.devices.contains_key(*id)).map(|(id, k)| (id.clone(), k.last_seen)).collect();
            by_age.sort_by_key(|(_, t)| *t);
            for (id, _) in by_age.into_iter().take(self.known.len() - MAX_KNOWN) {
                self.known.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_match_the_documented_ones() {
        let s = Settings::default();
        assert!(s.validate().is_ok());
        assert!(s.enabled && s.announce_connections && !s.announce_disconnections && s.announce_low_battery && s.announce_critical_battery);
        assert!(!s.announce_charging && !s.announce_fully_charged && !s.infer_charging && s.announce_unknown_usb);
        assert_eq!((s.low_percent, s.very_low_percent, s.critical_percent), (20, 10, 5));
    }

    #[test]
    fn thresholds_must_be_in_order_and_in_range() {
        let ok = |l, v, c| Settings { low_percent: l, very_low_percent: v, critical_percent: c, ..Settings::default() }.validate().is_ok();
        assert!(ok(30, 15, 3) && ok(20, 10, 5));
        assert!(!ok(10, 20, 5) && !ok(20, 20, 5) && !ok(20, 10, 10) && !ok(0, 10, 5) && !ok(100, 10, 5));
        let mut s = Settings::default();
        s.templates.insert("nope".into(), "x".into());
        assert!(s.validate().is_err());
        s.templates.clear();
        s.templates.insert("connected".into(), "x".repeat(201));
        assert!(s.validate().is_err());
        s.templates.insert("connected".into(), "{name} is here.".into());
        assert!(s.validate().is_ok());
        assert_eq!(s.template("connected"), "{name} is here.");
        assert_eq!(s.template("disconnected"), "{name} disconnected.");
    }

    #[test]
    fn the_state_file_round_trips_and_a_damaged_one_is_moved_aside() {
        let dir = std::env::temp_dir().join(format!("aqua_devices_state_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state.json");
        let mut st = State::default();
        st.settings.announce_disconnections = true;
        st.settings.devices.insert("bt:aa".into(), DevicePrefs { ignore: true, ..DevicePrefs::default() });
        st.known.insert("bt:aa".into(), Known { name: "Buds".into(), device_type: DeviceType::Headphones, connection: Connection::Bluetooth, id_kind: IdKind::Address, last_seen: 5 });
        st.save(&path).unwrap();
        let (back, warn) = State::load(&path);
        assert!(warn.is_none() && back == st);
        std::fs::write(&path, b"{ not json").unwrap();
        let (fresh, warn) = State::load(&path);
        assert!(warn.unwrap().contains("damaged") && fresh == State::default());
        assert!(path.with_extension("json.damaged").exists() && !path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_devices_are_dropped_but_never_one_with_preferences() {
        let mut st = State::default();
        let k = |t| Known { name: "x".into(), device_type: DeviceType::Mouse, connection: Connection::Usb, id_kind: IdKind::Weak, last_seen: t };
        let now = 400 * 86_400;
        st.known.insert("old".into(), k(1));
        st.known.insert("old-but-set".into(), k(1));
        st.known.insert("recent".into(), k(now - 100));
        st.settings.devices.insert("old-but-set".into(), DevicePrefs { ignore: true, ..DevicePrefs::default() });
        st.prune(now);
        assert_eq!(st.known.keys().cloned().collect::<Vec<_>>(), vec!["old-but-set".to_string(), "recent".to_string()]);
    }
}
