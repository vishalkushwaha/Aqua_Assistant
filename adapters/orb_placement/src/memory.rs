//! What Aqua remembers about where the person likes the orb: one preferred spot, a spot per program and (optionally) per
//! page. A spot is stored as a fraction of the screen, so it still makes sense on another monitor or after a resolution
//! change. Nothing about page content is kept: pages are one-way fingerprints of a title.

use crate::settings::Settings;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::geometry::Rect;

pub const MAX_APPS: usize = 40;
pub const MAX_PAGES: usize = 100;
const FILE_VERSION: u32 = 1;

/// A position on a screen's work area: 0.0 is the left or top edge, 1.0 the right or bottom edge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Spot {
    pub fx: f32,
    pub fy: f32,
}

impl Spot {
    pub fn from_rect(fp: &Rect, work: &Rect) -> Spot {
        let frac = |pos: i32, origin: i32, room: i32| if room <= 0 { 0.5 } else { ((pos - origin) as f32 / room as f32).clamp(0.0, 1.0) };
        Spot { fx: frac(fp.x, work.x, work.w - fp.w), fy: frac(fp.y, work.y, work.h - fp.h) }
    }

    /// The rectangle of the given size at this spot on `work`.
    pub fn to_rect(&self, size: (i32, i32), work: &Rect) -> Rect {
        let at = |f: f32, origin: i32, room: i32| origin + (f.clamp(0.0, 1.0) * room.max(0) as f32).round() as i32;
        Rect::new(at(self.fx, work.x, work.w - size.0), at(self.fy, work.y, work.h - size.1), size.0, size.1)
    }

    /// "bottom-right", "top-left", "middle"...
    pub fn label(&self) -> String {
        let h = if self.fx < 0.2 { "left" } else if self.fx > 0.8 { "right" } else { "center" };
        let v = if self.fy < 0.2 { "top" } else if self.fy > 0.8 { "bottom" } else { "middle" };
        match (v, h) {
            ("middle", "center") => "middle".into(),
            (v, "center") => format!("{v}-center"),
            (v, h) => format!("{v}-{h}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSpot {
    pub app: String,
    pub spot: Spot,
    pub seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageSpot {
    /// Hex of the salted page fingerprint.
    pub key: String,
    pub app: String,
    pub spot: Spot,
    pub seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub version: u32,
    pub salt: u64,
    pub settings: Settings,
    pub preferred: Option<Spot>,
    pub apps: Vec<AppSpot>,
    pub pages: Vec<PageSpot>,
}

impl Default for Saved {
    fn default() -> Self {
        Saved { version: FILE_VERSION, salt: 0, settings: Settings::default(), preferred: None, apps: Vec::new(), pages: Vec::new() }
    }
}

impl Saved {
    /// Read the file. A missing file is a fresh start; a damaged one is set aside (as `.bad`) and also a fresh start, with a note.
    pub fn load(path: &Path, seed: u64) -> (Saved, Option<String>) {
        let fresh = |salt: u64| Saved { salt, ..Saved::default() };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (fresh(seed), None),
            Err(e) => return (fresh(seed), Some(format!("could not read {}: {e}", path.display()))),
        };
        match serde_json::from_slice::<Saved>(&bytes) {
            Ok(mut s) => {
                if s.salt == 0 {
                    s.salt = seed;
                }
                s.apps.truncate(MAX_APPS);
                s.pages.truncate(MAX_PAGES);
                (s, None)
            }
            Err(e) => {
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                (fresh(seed), Some(format!("the saved orb positions could not be read ({e}); starting again, the old file was kept as .bad")))
            }
        }
    }

    /// Write the file in one piece (to a temporary name, then renamed over the old one).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    pub fn app_spot(&self, app: &str) -> Option<Spot> {
        self.apps.iter().find(|a| a.app == app).map(|a| a.spot)
    }

    pub fn page_spot(&self, key: &str) -> Option<Spot> {
        self.pages.iter().find(|p| p.key == key).map(|p| p.spot)
    }

    pub fn remember_app(&mut self, app: &str, spot: Spot, now_ms: u64) {
        match self.apps.iter_mut().find(|a| a.app == app) {
            Some(a) => {
                a.spot = spot;
                a.seen_ms = now_ms;
            }
            None => self.apps.push(AppSpot { app: app.to_string(), spot, seen_ms: now_ms }),
        }
        while self.apps.len() > MAX_APPS {
            if let Some(i) = self.apps.iter().enumerate().min_by_key(|(_, a)| a.seen_ms).map(|(i, _)| i) {
                self.apps.remove(i);
            }
        }
    }

    pub fn remember_page(&mut self, key: &str, app: &str, spot: Spot, now_ms: u64) {
        match self.pages.iter_mut().find(|p| p.key == key) {
            Some(p) => {
                p.spot = spot;
                p.seen_ms = now_ms;
            }
            None => self.pages.push(PageSpot { key: key.to_string(), app: app.to_string(), spot, seen_ms: now_ms }),
        }
        while self.pages.len() > MAX_PAGES {
            if let Some(i) = self.pages.iter().enumerate().min_by_key(|(_, p)| p.seen_ms).map(|(i, _)| i) {
                self.pages.remove(i);
            }
        }
    }

    /// Forget what was remembered for a program, including its pages. Returns whether anything was there.
    pub fn forget_app(&mut self, app: &str) -> bool {
        let before = self.apps.len() + self.pages.len();
        self.apps.retain(|a| a.app != app);
        self.pages.retain(|p| p.app != app);
        self.apps.len() + self.pages.len() != before
    }

    /// Forget every remembered spot (programs, pages and the preferred spot). The settings stay.
    pub fn forget_all(&mut self) {
        self.apps.clear();
        self.pages.clear();
        self.preferred = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua-orb-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("placement.json")
    }

    #[test]
    fn a_spot_means_the_same_place_on_another_screen() {
        let small = Rect::new(0, 0, 1366, 728);
        let big = Rect::new(0, 0, 2560, 1400);
        let orb = Rect::new(1366 - 220 - 16, 728 - 220 - 8, 220, 220);
        let s = Spot::from_rect(&orb, &small);
        assert_eq!(s.label(), "bottom-right");
        let there = s.to_rect((220, 220), &big);
        assert!(there.x > 2000 && there.y > 1000 && there.right() <= 2560 && there.bottom() <= 1400, "{there:?}");
        // And on a screen that does not start at 0, 0.
        let second = Rect::new(-1920, 0, 1920, 1040);
        let on_second = Spot { fx: 0.0, fy: 0.0 }.to_rect((220, 220), &second);
        assert_eq!((on_second.x, on_second.y), (-1920, 0));
    }

    #[test]
    fn spot_labels() {
        assert_eq!(Spot { fx: 0.0, fy: 0.0 }.label(), "top-left");
        assert_eq!(Spot { fx: 0.5, fy: 0.0 }.label(), "top-center");
        assert_eq!(Spot { fx: 1.0, fy: 0.5 }.label(), "middle-right");
        assert_eq!(Spot { fx: 0.5, fy: 0.5 }.label(), "middle");
    }

    #[test]
    fn a_spot_survives_a_screen_with_no_room() {
        // A work area smaller than the orb must not divide by zero or go negative.
        let tiny = Rect::new(10, 10, 100, 100);
        assert_eq!(Spot::from_rect(&Rect::new(10, 10, 220, 220), &tiny), Spot { fx: 0.5, fy: 0.5 });
        assert_eq!(Spot { fx: 1.0, fy: 1.0 }.to_rect((220, 220), &tiny), Rect::new(10, 10, 220, 220));
    }

    #[test]
    fn the_least_recently_used_entries_are_dropped() {
        let mut s = Saved::default();
        for i in 0..(MAX_APPS + 10) {
            s.remember_app(&format!("app{i}.exe"), Spot { fx: 0.1, fy: 0.1 }, i as u64);
        }
        assert_eq!(s.apps.len(), MAX_APPS);
        assert!(s.app_spot("app0.exe").is_none() && s.app_spot("app49.exe").is_some());
        for i in 0..(MAX_PAGES + 5) {
            s.remember_page(&format!("{i:016x}"), "chrome.exe", Spot { fx: 0.9, fy: 0.9 }, i as u64);
        }
        assert_eq!(s.pages.len(), MAX_PAGES);
        // Remembering again updates in place, it does not add.
        s.remember_page(&format!("{:016x}", MAX_PAGES + 4), "chrome.exe", Spot { fx: 0.0, fy: 0.0 }, 9999);
        assert_eq!(s.pages.len(), MAX_PAGES);
        assert_eq!(s.page_spot(&format!("{:016x}", MAX_PAGES + 4)), Some(Spot { fx: 0.0, fy: 0.0 }));
    }

    #[test]
    fn forgetting_a_program_forgets_its_pages_too() {
        let mut s = Saved::default();
        s.remember_app("chrome.exe", Spot { fx: 1.0, fy: 1.0 }, 1);
        s.remember_page("aa", "chrome.exe", Spot { fx: 0.0, fy: 0.0 }, 1);
        s.remember_page("bb", "code.exe", Spot { fx: 0.0, fy: 0.0 }, 1);
        assert!(s.forget_app("chrome.exe"));
        assert!(s.page_spot("aa").is_none() && s.page_spot("bb").is_some());
        assert!(!s.forget_app("chrome.exe"));
        s.preferred = Some(Spot { fx: 0.5, fy: 0.5 });
        s.settings.lock = true;
        s.forget_all();
        assert!(s.pages.is_empty() && s.preferred.is_none() && s.settings.lock, "settings are kept");
    }

    #[test]
    fn saving_and_loading_round_trips() {
        let path = temp("roundtrip");
        let mut s = Saved { salt: 77, ..Saved::default() };
        s.settings.sensitivity = crate::settings::Sensitivity::High;
        s.preferred = Some(Spot { fx: 1.0, fy: 1.0 });
        s.remember_app("code.exe", Spot { fx: 1.0, fy: 0.0 }, 5);
        s.save(&path).unwrap();
        let (loaded, note) = Saved::load(&path, 1);
        assert!(note.is_none());
        assert_eq!(loaded, s);
        assert!(!path.with_extension("json.tmp").exists(), "the temporary file is gone");
    }

    #[test]
    fn a_missing_file_is_a_fresh_start_and_a_damaged_one_is_set_aside() {
        let path = temp("damaged");
        let (fresh, note) = Saved::load(&path, 42);
        assert!(note.is_none() && fresh.salt == 42 && fresh.apps.is_empty());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ this is not json").unwrap();
        let (again, note) = Saved::load(&path, 43);
        assert!(note.unwrap().contains(".bad") && again.salt == 43);
        assert!(path.with_extension("json.bad").exists() && !path.exists());
    }

    #[test]
    fn an_oversized_file_is_trimmed_on_load() {
        let path = temp("big");
        let mut s = Saved { salt: 1, ..Saved::default() };
        for i in 0..(MAX_APPS + 20) {
            s.apps.push(AppSpot { app: format!("a{i}"), spot: Spot { fx: 0.0, fy: 0.0 }, seen_ms: i as u64 });
        }
        s.save(&path).unwrap();
        assert_eq!(Saved::load(&path, 1).0.apps.len(), MAX_APPS);
    }
}
