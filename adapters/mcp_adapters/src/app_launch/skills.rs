//! What Aqua has learned about starting apps: for each spoken name, the way that worked, how sure Aqua is of it, and how
//! long it took. This is a small reinforcement loop: a launch that is seen to work raises the confidence in its recipe, one
//! that fails lowers it, and a recipe that fails twice in a row is dropped so the next request finds a new way.
//!
//! Pure data and rules, no windows and no processes, so every rule is tested without starting anything.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::windows_mcp::guard::{evaluate, Caller, Verdict};

/// Most recipes kept; the one used least recently goes first.
pub const MAX_SKILLS: usize = 200;
/// A recipe below this confidence is not used (it is tried again from scratch).
pub const MIN_CONFIDENCE: f32 = 0.2;
/// Two failures in a row and the recipe is forgotten.
pub const MAX_FAIL_STREAK: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A program found through Windows' own search path (`notepad.exe`).
    Exe,
    /// The full path of a program or shortcut.
    Path,
    /// A Start menu entry (`shell:AppsFolder\<id>`), which also reaches Store apps.
    AppsFolder,
    /// A settings page (`ms-settings:`).
    Uri,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    pub kind: Kind,
    pub target: String,
}

impl Recipe {
    /// The string handed to the shell.
    pub fn shell_target(&self) -> String {
        match self.kind {
            Kind::AppsFolder => format!("shell:AppsFolder\\{}", self.target),
            _ => self.target.clone(),
        }
    }

    /// The program's file name without ".exe", when the recipe says it (used to recognise the window that appears).
    pub fn program_stem(&self) -> Option<String> {
        let name = match self.kind {
            Kind::Exe => self.target.as_str(),
            Kind::Path => Path::new(&self.target).file_name()?.to_str()?,
            _ => return None,
        };
        Some(name.rsplit_once('.').map_or(name, |(s, _)| s).to_ascii_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    /// The spoken name, normalised ("notepad", "file explorer").
    pub key: String,
    /// How it is shown and spoken ("Notepad").
    pub name: String,
    pub recipe: Recipe,
    /// Where the recipe came from: "well-known", "file index" or "start menu".
    pub source: String,
    pub confidence: f32,
    pub successes: u32,
    pub failures: u32,
    pub fail_streak: u32,
    /// Started but the window was not seen: neither a reward nor a penalty.
    pub unverified: u32,
    pub avg_ms: u32,
    pub last_ms: u32,
    pub learned_unix: u64,
    pub last_unix: u64,
}

impl Skill {
    pub fn new(key: &str, name: &str, recipe: Recipe, source: &str, now: u64) -> Skill {
        Skill { key: key.to_string(), name: name.to_string(), recipe, source: source.to_string(), confidence: 0.5, successes: 0, failures: 0, fail_streak: 0, unverified: 0, avg_ms: 0, last_ms: 0, learned_unix: now, last_unix: now }
    }

    pub fn usable(&self) -> bool {
        self.confidence >= MIN_CONFIDENCE && self.fail_streak < MAX_FAIL_STREAK
    }

    /// The launch worked and the window was seen.
    pub fn reward(&mut self, ms: u32, now: u64) {
        self.confidence += 0.25 * (1.0 - self.confidence);
        self.successes += 1;
        self.fail_streak = 0;
        self.last_ms = ms;
        self.avg_ms = if self.avg_ms == 0 { ms } else { (self.avg_ms * 3 + ms) / 4 };
        self.last_unix = now;
    }

    /// The launch failed outright.
    pub fn penalise(&mut self, now: u64) {
        self.confidence *= 0.5;
        self.failures += 1;
        self.fail_streak += 1;
        self.last_unix = now;
    }

    pub fn note_unverified(&mut self, now: u64) {
        self.unverified += 1;
        self.last_unix = now;
    }
}

/// "the Notepad app" -> "notepad". Words only, lower case, filler at the ends removed.
pub fn normalize(name: &str) -> String {
    let words: Vec<String> = name.split(|c: char| !c.is_alphanumeric() && c != '+').filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    let mut w: Vec<&str> = words.iter().map(String::as_str).collect();
    if w.first() == Some(&"the") {
        w.remove(0);
    }
    if w.len() > 1 && matches!(w.last(), Some(&"app") | Some(&"application") | Some(&"program")) {
        w.pop();
    }
    w.join(" ")
}

/// "file explorer" -> "File Explorer".
pub fn display(name: &str) -> String {
    name.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

const SHELL_LIKE: &[&str] = &["powershell", "pwsh", "cmd.exe", "windowsterminal", "\\wt.exe", "regedit", "taskmgr", "mmc.exe", "conhost", "wsl.exe", "bash.exe", "mintty", "openconsole"];

fn shell_like(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    SHELL_LIKE.iter().any(|p| l.contains(p)) || l == "wt.exe" || l == "cmd" || l == "wsl" || l == "bash"
}

/// Whether a recipe may be run. Recipes live in a file on disk, so they are checked every time they are used, not only when
/// they are made: shells and administration tools are refused by the same rule as for the agent, only plain program names or
/// absolute paths to programs and shortcuts are accepted, and nothing carries arguments.
pub fn recipe_valid(r: &Recipe) -> Result<(), String> {
    let t = r.target.trim();
    if t.is_empty() || t != r.target || t.chars().any(|c| c.is_control()) || t.contains("..") {
        return Err("the recipe is empty or contains something unsafe".into());
    }
    if shell_like(t) {
        return Err("it is a shell or administration tool".into());
    }
    match r.kind {
        Kind::Exe => {
            let ok = t.len() <= 40 && t.to_ascii_lowercase().ends_with(".exe") && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if !ok {
                return Err("not a plain program name".into());
            }
        }
        Kind::Path => {
            let p = Path::new(t);
            let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
            if !p.is_absolute() || t.starts_with("\\\\") || t.len() > 259 || !matches!(ext.as_deref(), Some("exe") | Some("lnk")) {
                return Err("not an absolute path to a program or shortcut".into());
            }
        }
        Kind::AppsFolder => {
            let ok = t.len() <= 300 && t.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '!' | '{' | '}' | '-' | ' ' | '\\' | '(' | ')' | '+'));
            if !ok {
                return Err("not a Start menu identifier".into());
            }
        }
        Kind::Uri => {
            if !t.to_ascii_lowercase().starts_with("ms-settings:") || t.len() > 100 || t.contains(' ') {
                return Err("only settings pages can be opened this way".into());
            }
        }
    }
    // The same rule the agent is held to for starting an app by name.
    let stem = Path::new(t).file_stem().and_then(|s| s.to_str()).unwrap_or(t);
    if let Verdict::Deny(_, why) = evaluate("App", &json!({"mode": "launch", "name": stem}), Caller::Voice, None) {
        return Err(why);
    }
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub version: u32,
    pub skills: Vec<Skill>,
}

impl Store {
    /// A missing file is a fresh start; a damaged one is set aside as `.bad`.
    pub fn load(path: &Path) -> Store {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<Store>(&bytes).unwrap_or_else(|_| {
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                Store::default()
            }),
            Err(_) => Store::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        aqua_tool_host::fsutil::write_atomic_durable(path, &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
    }

    pub fn get(&self, key: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.key == key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Skill> {
        self.skills.iter_mut().find(|s| s.key == key)
    }

    pub fn put(&mut self, skill: Skill) {
        self.version = 1;
        match self.skills.iter_mut().find(|s| s.key == skill.key) {
            Some(old) => *old = skill,
            None => self.skills.push(skill),
        }
        while self.skills.len() > MAX_SKILLS {
            if let Some(i) = self.skills.iter().enumerate().min_by_key(|(_, s)| s.last_unix).map(|(i, _)| i) {
                self.skills.remove(i);
            }
        }
    }

    pub fn forget(&mut self, key: &str) -> bool {
        let before = self.skills.len();
        self.skills.retain(|s| s.key != key);
        self.skills.len() != before
    }
}

/// `Get-StartApps | ConvertTo-Csv` output: `"Name","AppID"` per line after a header.
pub fn parse_start_apps(csv: &str) -> Vec<(String, String)> {
    csv.lines()
        .skip(1)
        .filter_map(|l| {
            let l = l.trim().strip_prefix('"')?.strip_suffix('"')?;
            let (name, id) = l.split_once("\",\"")?;
            (!name.is_empty() && !id.is_empty()).then(|| (name.to_string(), id.to_string()))
        })
        .collect()
}

/// The Start menu entry a spoken name means: an exact match, or the only entry that contains it. Anything unclear is not learned.
pub fn best_start_app(key: &str, apps: &[(String, String)]) -> Option<(String, String)> {
    let key_flat: String = key.chars().filter(|c| c.is_alphanumeric()).collect();
    if key_flat.len() < 3 {
        return None;
    }
    let flat = |s: &str| -> String { normalize(s).chars().filter(|c| c.is_alphanumeric()).collect() };
    if let Some(exact) = apps.iter().find(|(n, _)| flat(n) == key_flat) {
        return Some(exact.clone());
    }
    let contains: Vec<&(String, String)> = apps.iter().filter(|(n, _)| flat(n).contains(&key_flat)).collect();
    if contains.len() == 1 {
        return Some(contains[0].clone());
    }
    None
}

/// A spoken name's well-known recipe: programs that ship with Windows. Everything else is learned.
pub fn well_known(key: &str) -> Option<(&'static str, Recipe)> {
    let exe = |n: &str| Recipe { kind: Kind::Exe, target: n.to_string() };
    Some(match key {
        "notepad" => ("Notepad", exe("notepad.exe")),
        "calculator" | "calc" => ("Calculator", exe("calc.exe")),
        "paint" | "mspaint" => ("Paint", exe("mspaint.exe")),
        "file explorer" | "explorer" | "windows explorer" | "files" => ("File Explorer", exe("explorer.exe")),
        "snipping tool" => ("Snipping Tool", exe("snippingtool.exe")),
        "wordpad" => ("WordPad", exe("wordpad.exe")),
        "settings" | "windows settings" => ("Settings", Recipe { kind: Kind::Uri, target: "ms-settings:".to_string() }),
        _ => return None,
    })
}

pub fn default_path() -> PathBuf {
    PathBuf::from("data").join("learning").join("launch_skills.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(n: &str) -> Recipe {
        Recipe { kind: Kind::Exe, target: n.into() }
    }

    #[test]
    fn spoken_names_are_normalised() {
        assert_eq!(normalize("Notepad"), "notepad");
        assert_eq!(normalize("the Notepad app"), "notepad");
        assert_eq!(normalize("File  Explorer!"), "file explorer");
        assert_eq!(normalize("Notepad++"), "notepad++");
        assert_eq!(normalize("app"), "app", "a single word is never emptied");
        assert_eq!(display("file explorer"), "File Explorer");
    }

    #[test]
    fn a_launch_that_works_raises_confidence_and_tracks_the_time() {
        let mut s = Skill::new("notepad", "Notepad", exe("notepad.exe"), "well-known", 1);
        assert!(s.usable() && (s.confidence - 0.5).abs() < 1e-6);
        s.reward(800, 2);
        s.reward(400, 3);
        assert!(s.confidence > 0.7 && s.confidence < 1.0, "{}", s.confidence);
        assert_eq!((s.successes, s.fail_streak, s.last_ms), (2, 0, 400));
        assert_eq!(s.avg_ms, (800 * 3 + 400) / 4);
        for _ in 0..30 {
            s.reward(300, 4);
        }
        assert!(s.confidence <= 1.0);
    }

    #[test]
    fn two_failures_in_a_row_make_a_recipe_unusable_but_one_success_resets_the_streak() {
        let mut s = Skill::new("x", "X", exe("x.exe"), "start menu", 1);
        s.penalise(2);
        assert!(s.usable(), "one failure is forgiven");
        s.reward(500, 3);
        s.penalise(4);
        assert!(s.usable(), "the streak was reset by the success");
        s.penalise(5);
        assert!(!s.usable(), "two in a row");
        assert_eq!((s.failures, s.fail_streak), (3, 2));
    }

    #[test]
    fn a_low_confidence_recipe_is_not_used() {
        let mut s = Skill::new("x", "X", exe("x.exe"), "start menu", 1);
        s.confidence = 0.15;
        assert!(!s.usable());
    }

    #[test]
    fn unverified_launches_change_neither_confidence_nor_streak() {
        let mut s = Skill::new("x", "X", exe("x.exe"), "start menu", 1);
        s.note_unverified(2);
        assert_eq!((s.unverified, s.failures, s.successes), (1, 0, 0));
        assert!((s.confidence - 0.5).abs() < 1e-6);
    }

    #[test]
    fn recipes_that_are_safe_are_accepted_and_the_rest_are_refused() {
        let ok = [
            exe("notepad.exe"),
            Recipe { kind: Kind::Path, target: "C:\\Program Files\\Microsoft VS Code\\Code.exe".into() },
            Recipe { kind: Kind::Path, target: "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\Paint.lnk".into() },
            Recipe { kind: Kind::AppsFolder, target: "Microsoft.WindowsNotepad_8wekyb3d8bbwe!App".into() },
            Recipe { kind: Kind::AppsFolder, target: "{6D809377-6AF0-444B-8957-A3773F02200E}\\Microsoft VS Code\\Code.exe".into() },
            Recipe { kind: Kind::Uri, target: "ms-settings:".into() },
            Recipe { kind: Kind::Uri, target: "ms-settings:bluetooth".into() },
        ];
        for r in &ok {
            assert!(recipe_valid(r).is_ok(), "{r:?}: {:?}", recipe_valid(r));
        }
        let bad = [
            exe("cmd.exe"),
            exe("powershell.exe"),
            exe("..\\evil.exe"),
            exe("notepad.exe /c calc"),
            exe("notepad"),
            Recipe { kind: Kind::Path, target: "relative\\app.exe".into() },
            Recipe { kind: Kind::Path, target: "\\\\server\\share\\app.exe".into() },
            Recipe { kind: Kind::Path, target: "C:\\Users\\me\\run.bat".into() },
            Recipe { kind: Kind::Path, target: "C:\\Windows\\System32\\cmd.exe".into() },
            Recipe { kind: Kind::Path, target: "C:\\a\\..\\b\\app.exe".into() },
            Recipe { kind: Kind::AppsFolder, target: "x\"; calc".into() },
            Recipe { kind: Kind::AppsFolder, target: "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App".into() },
            Recipe { kind: Kind::Uri, target: "https://example.com".into() },
            Recipe { kind: Kind::Uri, target: "ms-settings: calc".into() },
            exe(""),
        ];
        for r in &bad {
            assert!(recipe_valid(r).is_err(), "{r:?} should be refused");
        }
    }

    #[test]
    fn the_store_round_trips_replaces_forgets_and_keeps_to_its_limit() {
        let dir = std::env::temp_dir().join(format!("aqua-launch-test-{}", std::process::id()));
        let path = dir.join("launch_skills.json");
        let _ = std::fs::remove_dir_all(&dir);
        let mut st = Store::default();
        st.put(Skill::new("notepad", "Notepad", exe("notepad.exe"), "well-known", 1));
        let mut again = Skill::new("notepad", "Notepad", exe("notepad.exe"), "well-known", 1);
        again.reward(300, 9);
        st.put(again);
        assert_eq!(st.skills.len(), 1, "same key replaces");
        st.save(&path).unwrap();
        let loaded = Store::load(&path);
        assert_eq!(loaded.get("notepad").unwrap().successes, 1);
        assert!(!path.with_extension("json.tmp").exists());
        let mut st = loaded;
        for i in 0..(MAX_SKILLS + 20) {
            st.put(Skill::new(&format!("app{i}"), "A", exe("a.exe"), "x", i as u64 + 10));
        }
        assert_eq!(st.skills.len(), MAX_SKILLS);
        assert!(st.get("app219").is_some() && st.get("app0").is_none(), "least recently used goes first");
        assert!(st.forget("app219") && !st.forget("app219"));
    }

    #[test]
    fn a_damaged_file_is_set_aside() {
        let dir = std::env::temp_dir().join(format!("aqua-launch-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("launch_skills.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(Store::load(&path).skills.is_empty());
        assert!(path.with_extension("json.bad").exists());
        assert!(Store::load(&dir.join("missing.json")).skills.is_empty());
    }

    #[test]
    fn start_menu_output_is_parsed_and_only_a_clear_match_is_learned() {
        let csv = "\"Name\",\"AppID\"\r\n\"Notepad\",\"Microsoft.WindowsNotepad_8wekyb3d8bbwe!App\"\r\n\"Visual Studio Code\",\"{6D809377}\\Microsoft VS Code\\Code.exe\"\r\n\"Code Blocks\",\"X!Y\"\r\n\"Paint\",\"Microsoft.Paint_8wekyb3d8bbwe!App\"\r\n\"Paint 3D\",\"Microsoft.MSPaint_8wekyb3d8bbwe!Microsoft.MSPaint\"\r\n";
        let apps = parse_start_apps(csv);
        assert_eq!(apps.len(), 5);
        assert_eq!(best_start_app("notepad", &apps).unwrap().1, "Microsoft.WindowsNotepad_8wekyb3d8bbwe!App");
        assert_eq!(best_start_app("paint", &apps).unwrap().0, "Paint", "an exact name beats a longer one that contains it");
        assert_eq!(best_start_app("visual studio", &apps).unwrap().0, "Visual Studio Code", "the only entry containing the words");
        assert!(best_start_app("code", &apps).is_none(), "two entries contain it: not guessed");
        assert!(best_start_app("xy", &apps).is_none(), "too short");
        assert!(best_start_app("spotify", &apps).is_none());
    }

    #[test]
    fn the_well_known_programs_are_valid_recipes() {
        for key in ["notepad", "calculator", "paint", "file explorer", "snipping tool", "wordpad", "settings"] {
            let (_, r) = well_known(key).unwrap_or_else(|| panic!("{key}"));
            assert!(recipe_valid(&r).is_ok(), "{key}: {r:?}");
        }
        assert!(well_known("cmd").is_none() && well_known("powershell").is_none() && well_known("control panel").is_none(), "what the guard refuses is not a shortcut");
        assert_eq!(well_known("notepad").unwrap().1.program_stem().as_deref(), Some("notepad"));
        assert_eq!(Recipe { kind: Kind::AppsFolder, target: "A!B".into() }.shell_target(), "shell:AppsFolder\\A!B");
    }
}
