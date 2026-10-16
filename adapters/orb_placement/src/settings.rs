//! What the person can switch, and how it is changed from the admin panel.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How often the window may be looked at while the wheel is turning (milliseconds).
pub const SCROLL_INTERVAL_MIN: u32 = 150;
pub const SCROLL_INTERVAL_MAX: u32 = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    /// Moves only when the orb is badly in the way.
    Low,
    Medium,
    /// Moves as soon as the orb covers a little content.
    High,
}

impl Sensitivity {
    /// The share of the orb that may sit on content before the spot counts as obstructed.
    pub fn obstructed_above(self) -> f32 {
        match self {
            Sensitivity::Low => 0.40,
            Sensitivity::Medium => 0.22,
            Sensitivity::High => 0.10,
        }
    }

    /// How many score points a new spot must beat the current one by before the orb moves (the hysteresis).
    pub fn margin(self) -> f32 {
        match self {
            Sensitivity::Low => 60.0,
            Sensitivity::Medium => 30.0,
            Sensitivity::High => 15.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Move the orb out of the way when the active window or page changes. Off: the orb only moves when you move it.
    pub auto_move: bool,
    /// The orb never moves by itself (you can still drag it).
    pub lock: bool,
    pub avoid_content: bool,
    pub avoid_cursor: bool,
    /// Remember where you put the orb for each program.
    pub per_app: bool,
    /// Remember where you put the orb for each page or document (by a one-way fingerprint of its title).
    pub per_page: bool,
    /// Look at the window again when the mouse wheel is used, in case text has scrolled under the orb.
    pub scroll_move: bool,
    /// Also look while the wheel is still turning (at most every `scroll_interval_ms`), not only once it stops.
    pub scroll_live: bool,
    pub scroll_interval_ms: u32,
    pub sensitivity: Sensitivity,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto_move: true, lock: false, avoid_content: true, avoid_cursor: true, per_app: true, per_page: false, scroll_move: true, scroll_live: true, scroll_interval_ms: 350, sensitivity: Sensitivity::Medium }
    }
}

impl Settings {
    /// Whether the orb may move by itself right now.
    pub fn may_auto_move(&self) -> bool {
        self.auto_move && !self.lock
    }

    /// Change settings from a JSON object (`{"lock": true, "sensitivity": "high"}`). All or nothing: one bad field changes nothing.
    /// Returns the names of the settings that changed.
    pub fn apply_patch(&mut self, patch: &Value) -> Result<Vec<&'static str>, String> {
        let obj = patch.as_object().ok_or("the change must be a JSON object")?;
        let mut next = self.clone();
        for (key, value) in obj {
            let flag = || value.as_bool().ok_or_else(|| format!("\"{key}\" must be true or false"));
            match key.as_str() {
                "auto_move" => next.auto_move = flag()?,
                "lock" => next.lock = flag()?,
                "avoid_content" => next.avoid_content = flag()?,
                "avoid_cursor" => next.avoid_cursor = flag()?,
                "per_app" => next.per_app = flag()?,
                "per_page" => next.per_page = flag()?,
                "scroll_move" => next.scroll_move = flag()?,
                "scroll_live" => next.scroll_live = flag()?,
                "scroll_interval_ms" => {
                    next.scroll_interval_ms = value.as_u64().filter(|n| (u64::from(SCROLL_INTERVAL_MIN)..=u64::from(SCROLL_INTERVAL_MAX)).contains(n)).ok_or_else(|| format!("\"scroll_interval_ms\" must be a whole number from {SCROLL_INTERVAL_MIN} to {SCROLL_INTERVAL_MAX}"))? as u32
                }
                "sensitivity" => {
                    next.sensitivity = match value.as_str() {
                        Some("low") => Sensitivity::Low,
                        Some("medium") => Sensitivity::Medium,
                        Some("high") => Sensitivity::High,
                        _ => return Err("\"sensitivity\" must be \"low\", \"medium\" or \"high\"".into()),
                    }
                }
                other => return Err(format!("unknown setting \"{other}\"")),
            }
        }
        let mut changed = Vec::new();
        macro_rules! diff {
            ($($f:ident),*) => { $( if next.$f != self.$f { changed.push(stringify!($f)); } )* };
        }
        diff!(auto_move, lock, avoid_content, avoid_cursor, per_app, per_page, scroll_move, scroll_live, scroll_interval_ms, sensitivity);
        *self = next;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_move_the_orb_automatically_and_remember_programs_but_not_pages() {
        let s = Settings::default();
        assert!(s.may_auto_move() && s.avoid_content && s.avoid_cursor && s.per_app && !s.per_page && !s.lock && s.scroll_move && s.scroll_live && s.scroll_interval_ms == 350);
        assert_eq!(s.sensitivity, Sensitivity::Medium);
    }

    #[test]
    fn a_patch_changes_only_what_it_names_and_reports_it() {
        let mut s = Settings::default();
        let changed = s.apply_patch(&json!({"lock": true, "sensitivity": "high", "per_app": true})).unwrap();
        assert_eq!(changed, vec!["lock", "sensitivity"]);
        assert!(s.lock && !s.may_auto_move());
        assert_eq!(s.sensitivity, Sensitivity::High);
    }

    #[test]
    fn the_scroll_interval_must_be_in_range() {
        let mut s = Settings::default();
        assert_eq!(s.apply_patch(&json!({"scroll_interval_ms": 700})).unwrap(), vec!["scroll_interval_ms"]);
        assert_eq!(s.scroll_interval_ms, 700);
        for bad in [json!({"scroll_interval_ms": 10}), json!({"scroll_interval_ms": 99999}), json!({"scroll_interval_ms": "fast"}), json!({"scroll_interval_ms": 350.5})] {
            assert!(s.apply_patch(&bad).is_err(), "{bad}");
        }
        assert_eq!(s.scroll_interval_ms, 700);
    }

    #[test]
    fn a_bad_field_changes_nothing() {
        let mut s = Settings::default();
        assert!(s.apply_patch(&json!({"lock": true, "sensitivity": "extreme"})).is_err());
        assert!(s.apply_patch(&json!({"lock": "yes"})).is_err());
        assert!(s.apply_patch(&json!({"colour": true})).is_err());
        assert!(s.apply_patch(&json!([1, 2])).is_err());
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn a_settings_file_from_an_older_version_still_loads() {
        let s: Settings = serde_json::from_str(r#"{"lock": true}"#).unwrap();
        assert!(s.lock && s.avoid_content);
    }

    #[test]
    fn lower_sensitivity_needs_more_in_the_way() {
        assert!(Sensitivity::Low.obstructed_above() > Sensitivity::Medium.obstructed_above());
        assert!(Sensitivity::Medium.obstructed_above() > Sensitivity::High.obstructed_above());
        assert!(Sensitivity::Low.margin() > Sensitivity::High.margin());
    }
}
