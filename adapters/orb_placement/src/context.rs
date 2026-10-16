//! Which window and page the person is on, and whether a change is worth looking at again.

use crate::geometry::Rect;

/// What identifies "where the person is" for the purpose of placing the orb. The title is never kept, only a salted hash.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSig {
    /// The program's file name, lower case ("chrome.exe").
    pub app: String,
    pub hwnd: i64,
    pub page: u64,
    pub rect: Rect,
    pub monitor: String,
    pub fullscreen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    AppSwitched,
    PageChanged,
    WindowResized,
    WindowMoved,
    MonitorChanged,
    FullscreenChanged,
    DisplayChanged,
    SettingsChanged,
    /// The mouse wheel was used: text may have moved under the orb.
    Scrolled,
    /// A second look a moment after a switch, because pages often finish painting after their title changes.
    Settled,
    Manual,
}

impl Why {
    pub fn text(self) -> &'static str {
        match self {
            Why::AppSwitched => "you switched program",
            Why::PageChanged => "the page or document changed",
            Why::WindowResized => "the window changed size",
            Why::WindowMoved => "the window moved",
            Why::MonitorChanged => "the window moved to another screen",
            Why::FullscreenChanged => "full screen was switched",
            Why::DisplayChanged => "the screens or taskbar changed",
            Why::SettingsChanged => "a setting changed",
            Why::Scrolled => "you scrolled",
            Why::Settled => "the page had a moment to finish loading",
            Why::Manual => "you asked for a re-check",
        }
    }
}

/// Is the new situation different enough from the last one that the orb's spot needs checking again? Small things
/// (a counter in the title, a window nudged a few pixels) are not.
pub fn change(prev: Option<&ContextSig>, new: &ContextSig) -> Option<Why> {
    let Some(p) = prev else { return Some(Why::AppSwitched) };
    if p.hwnd != new.hwnd || p.app != new.app {
        return Some(Why::AppSwitched);
    }
    if p.monitor != new.monitor {
        return Some(Why::MonitorChanged);
    }
    if p.fullscreen != new.fullscreen {
        return Some(Why::FullscreenChanged);
    }
    if p.page != new.page {
        return Some(Why::PageChanged);
    }
    let (dw, dh) = ((p.rect.w - new.rect.w).abs(), (p.rect.h - new.rect.h).abs());
    if dw * 100 > p.rect.w.max(1) * 15 || dh * 100 > p.rect.h.max(1) * 15 {
        return Some(Why::WindowResized);
    }
    let moved = p.rect.distance(&new.rect);
    if moved > (p.rect.w / 10).max(120) as f32 {
        return Some(Why::WindowMoved);
    }
    None
}

const BROWSERS: &[&str] = &["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "vivaldi.exe", "arc.exe", "iexplore.exe", "chromium.exe"];
const BROWSER_WORDS: &[&str] = &["chrome", "edge", "firefox", "brave", "opera", "vivaldi", "chromium", "internet explorer"];

pub fn is_browser(app: &str) -> bool {
    BROWSERS.contains(&app)
}

/// The title with the parts that change without the page changing taken off: an unread counter in front, an unsaved
/// marker, the browser's name at the end. Lower case, single spaces, at most 200 characters.
pub fn normalize_title(app: &str, title: &str) -> String {
    let mut t = title.trim();
    // "(3) Inbox", "(99+) Feed"
    if let Some(rest) = t.strip_prefix('(') {
        if let Some(end) = rest.find(')') {
            let inner = &rest[..end];
            if !inner.is_empty() && inner.len() <= 4 && inner.chars().all(|c| c.is_ascii_digit() || c == '+') {
                t = rest[end + 1..].trim_start();
            }
        }
    }
    t = t.trim_start_matches(|c: char| c == '*' || c == '\u{25cf}' || c == '\u{2022}' || c == ' ');
    if is_browser(app) {
        for sep in [" - ", " \u{2013} ", " \u{2014} "] {
            if let Some(i) = t.rfind(sep) {
                let tail = t[i + sep.len()..].to_lowercase();
                if BROWSER_WORDS.iter().any(|w| tail.contains(w)) {
                    t = &t[..i];
                    break;
                }
            }
        }
    }
    let collapsed: String = t.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    collapsed.chars().take(200).collect()
}

/// A salted 64-bit fingerprint of the program and the normalised title. It cannot be turned back into the title.
pub fn page_hash(salt: u64, app: &str, title: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325 ^ salt;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    eat(app.as_bytes());
    eat(&[0]);
    eat(normalize_title(app, title).as_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(app: &str, hwnd: i64, page: u64, rect: Rect) -> ContextSig {
        ContextSig { app: app.into(), hwnd, page, rect, monitor: "A".into(), fullscreen: false }
    }
    const R: Rect = Rect::new(0, 0, 1920, 1040);

    #[test]
    fn what_counts_as_a_change() {
        let base = sig("chrome.exe", 1, 10, R);
        assert_eq!(change(None, &base), Some(Why::AppSwitched));
        assert_eq!(change(Some(&base), &base), None);
        assert_eq!(change(Some(&base), &sig("code.exe", 2, 10, R)), Some(Why::AppSwitched));
        assert_eq!(change(Some(&base), &sig("chrome.exe", 3, 10, R)), Some(Why::AppSwitched), "another window of the same program");
        assert_eq!(change(Some(&base), &sig("chrome.exe", 1, 11, R)), Some(Why::PageChanged));
        assert_eq!(change(Some(&base), &sig("chrome.exe", 1, 10, Rect::new(0, 0, 1000, 700))), Some(Why::WindowResized));
        assert_eq!(change(Some(&base), &sig("chrome.exe", 1, 10, Rect::new(0, 0, 1900, 1030))), None, "a small resize is ignored");
        assert_eq!(change(Some(&base), &sig("chrome.exe", 1, 10, Rect::new(3, 2, 1920, 1040))), None, "a nudge is ignored");
        let small = sig("notepad.exe", 5, 1, Rect::new(100, 100, 600, 400));
        assert_eq!(change(Some(&small), &sig("notepad.exe", 5, 1, Rect::new(900, 300, 600, 400))), Some(Why::WindowMoved));
        let mut other = base.clone();
        other.monitor = "B".into();
        assert_eq!(change(Some(&base), &other), Some(Why::MonitorChanged));
        let mut full = base.clone();
        full.fullscreen = true;
        assert_eq!(change(Some(&base), &full), Some(Why::FullscreenChanged));
    }

    #[test]
    fn titles_are_normalised_so_noise_does_not_look_like_a_new_page() {
        assert_eq!(normalize_title("chrome.exe", "(3) Instagram - Google Chrome"), "instagram");
        assert_eq!(normalize_title("chrome.exe", "(12)   Instagram - Google Chrome"), "instagram");
        assert_eq!(normalize_title("msedge.exe", "Inbox \u{2013} Mail \u{2013} Microsoft Edge"), "inbox \u{2013} mail");
        assert_eq!(normalize_title("code.exe", "\u{25cf} main.rs - aqua - Visual Studio Code"), "main.rs - aqua - visual studio code");
        assert_eq!(normalize_title("notepad.exe", "*notes.txt - Notepad"), "notes.txt - notepad");
        assert_eq!(normalize_title("chrome.exe", "Chrome tips - a blog"), "chrome tips - a blog", "only the browser name at the end is removed");
        assert_eq!(normalize_title("chrome.exe", &"x".repeat(500)).len(), 200);
    }

    #[test]
    fn the_page_fingerprint_is_stable_salted_and_ignores_noise() {
        let a = page_hash(1, "chrome.exe", "(2) Instagram - Google Chrome");
        assert_eq!(a, page_hash(1, "chrome.exe", "Instagram - Google Chrome"));
        assert_ne!(a, page_hash(2, "chrome.exe", "Instagram - Google Chrome"), "another install has another salt");
        assert_ne!(a, page_hash(1, "chrome.exe", "Gmail - Google Chrome"));
        assert_ne!(a, page_hash(1, "msedge.exe", "Instagram - Google Chrome"));
    }
}
