//! Tiers of desktop control, by the kind of program in front (card R6-34).
//!
//! What the screen may be used for depends on what is on it, as in Claude Desktop's computer use:
//!
//! | In front | What desktop control may do |
//! |---|---|
//! | a **browser** | look only. A page is driven by Aqua's own browser tools, which know what a page is; clicking and typing through the screen would bypass them |
//! | a **terminal** (PowerShell, cmd, Windows Terminal, bash, WSL ...) | **click only**, and only a plain left click (focus a window, press a button of it). Nothing is typed or pressed in it, because typing is running; a command is run with the host's terminal tool, which asks and shows all of it |
//! | an **administration or command surface** (the Run box, Start search, the registry editor, a management console, the Task Manager) | look only |
//! | anything else | as before: the questions of the guard (Enter, Ctrl+Enter, an unknown window), and the refusals |
//!
//! These are refusals, not questions: they hold under full access too, and they are not asked about. `AQUA_DESKTOP_TIERS=relaxed` brings back what the guard did before this card (the
//! person is asked before input to a browser or a shell), for a person who wants it; it is the person's switch and is not set by default.
//!
//! The tiers are one layer over [`super::guard::evaluate_with`]: they only ever say no, so every refusal and every question of the guard stands whatever this says (a plain click in a
//! terminal is still asked about: input aimed at a shell is never sent silently).

use super::guard::{foreground_is_browser, foreground_is_shell, is_drag, Foreground};
use serde_json::Value;

pub const POLICY_ENV: &str = "AQUA_DESKTOP_TIERS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// The tiers hold (the default).
    Strict,
    /// What the guard did before the tiers: questions, not refusals, for a browser and a shell.
    Relaxed,
}

impl Policy {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("relaxed" | "ask" | "off") => Self::Relaxed,
            _ => Self::Strict,
        }
    }

    pub fn from_env() -> Self {
        Self::parse(std::env::var(POLICY_ENV).ok().as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppClass {
    Browser,
    Terminal,
    /// The Run box, Start search, the registry editor, a management console, the Task Manager: a place where text or a click starts a program or changes the system.
    AdminSurface,
    Other,
}

/// Programs whose window is a place to type commands.
const TERMINALS: &[&str] = &["powershell", "pwsh", "cmd", "windowsterminal", "wt", "conhost", "bash", "wsl", "mintty", "powershell_ise", "openconsole", "alacritty", "wezterm-gui"];

pub fn class_of(fg: &Foreground) -> AppClass {
    let lower = fg.exe.to_ascii_lowercase();
    let exe = lower.trim_end_matches(".exe");
    if TERMINALS.contains(&exe) {
        AppClass::Terminal
    } else if foreground_is_shell(fg) {
        AppClass::AdminSurface
    } else if foreground_is_browser(fg) {
        AppClass::Browser
    } else {
        AppClass::Other
    }
}

/// What a tier says about one call, over what the guard says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Override {
    /// The tier has nothing to add.
    None,
    /// Refused, with the reason the agent is given.
    Deny(String),
}

/// The calls that act on the screen. (A `Move` is one only as a drag; the rest only look.)
fn acting(tool: &str, args: &Value) -> Option<&'static str> {
    ["Click", "MultiSelect", "Type", "MultiEdit", "Shortcut"].into_iter().find(|t| t.eq_ignore_ascii_case(tool)).or_else(|| (tool.eq_ignore_ascii_case("Move") && is_drag("Move", args)).then_some("Move"))
}

/// A click that is only a click: the left button, once. A right click pastes in some terminals, and a double click selects a word that a later paste may use.
fn is_plain_left_click(args: &Value) -> bool {
    let button_ok = match args.get("button") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty() || s.trim().eq_ignore_ascii_case("left"),
        Some(_) => false,
    };
    let clicks_ok = match args.get("clicks") {
        None | Some(Value::Null) => true,
        Some(Value::Number(n)) => n.as_f64() == Some(1.0),
        Some(Value::String(s)) => s.trim() == "1",
        Some(_) => false,
    };
    button_ok && clicks_ok
}

/// What the tier of the window in front says about this call. `fg` of `None` is the guard's business (it asks), and a call that only looks is never refused.
pub fn tier_override(tool: &str, args: &Value, fg: Option<&Foreground>, policy: Policy) -> Override {
    if policy == Policy::Relaxed {
        return Override::None;
    }
    let (Some(fg), Some(action)) = (fg, acting(tool, args)) else { return Override::None };
    let window = if fg.title.is_empty() { fg.exe.clone() } else { format!("{} (\"{}\")", fg.exe, fg.title) };
    match class_of(fg) {
        AppClass::Other => Override::None,
        AppClass::Browser => Override::Deny(format!(
            "{action} in {window} is refused: a browser is view-only for desktop control, so the screen may be read and nothing is clicked or typed in it. Use Aqua's browser tools for a page (browser_navigate, browser_snapshot, browser_click, browser_type)."
        )),
        AppClass::AdminSurface => Override::Deny(format!("{action} in {window} is refused: a command or administration window is view-only for desktop control.")),
        AppClass::Terminal => match action {
            // Allowed by the tier, and still asked about by the guard: the tier never drops a question.
            "Click" if is_plain_left_click(args) => Override::None,
            "Click" => Override::Deny(format!("{action} in {window} is refused: in a terminal only a plain left click is allowed (a right click pastes in some of them).")),
            _ => Override::Deny(format!(
                "{action} in {window} is refused: a terminal is click-only for desktop control, so nothing is typed or pressed in it. To run a command use the host's terminal tool, which asks and shows all of the command."
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fg(exe: &str, title: &str) -> Foreground {
        Foreground { exe: exe.into(), title: title.into(), window: 0 }
    }

    fn deny(o: &Override) -> bool {
        matches!(o, Override::Deny(_))
    }

    #[test]
    fn the_program_in_front_is_a_browser_a_terminal_a_command_surface_or_something_else() {
        for (exe, title, class) in [
            ("chrome.exe", "Docs", AppClass::Browser),
            ("msedge.exe", "Inbox", AppClass::Browser),
            ("Firefox.exe", "x", AppClass::Browser),
            ("powershell.exe", "Windows PowerShell", AppClass::Terminal),
            ("WindowsTerminal.exe", "Terminal", AppClass::Terminal),
            ("cmd.exe", "C:\\", AppClass::Terminal),
            ("wsl.exe", "Ubuntu", AppClass::Terminal),
            ("mintty.exe", "~", AppClass::Terminal),
            ("regedit.exe", "Registry Editor", AppClass::AdminSurface),
            ("mmc.exe", "Services", AppClass::AdminSurface),
            ("taskmgr.exe", "Task Manager", AppClass::AdminSurface),
            ("explorer.exe", "Run", AppClass::AdminSurface),
            ("SearchHost.exe", "Search", AppClass::AdminSurface),
            ("explorer.exe", "Downloads", AppClass::Other),
            ("notepad.exe", "Untitled", AppClass::Other),
            ("code.exe", "main.rs - Visual Studio Code", AppClass::Other),
        ] {
            assert_eq!(class_of(&fg(exe, title)), class, "{exe} {title}");
        }
    }

    #[test]
    fn a_browser_and_a_command_surface_are_view_only_and_a_plain_app_is_left_to_the_guard() {
        for (exe, title) in [("chrome.exe", "Inbox - Gmail"), ("msedge.exe", "x"), ("regedit.exe", "Registry Editor"), ("explorer.exe", "Run")] {
            let f = fg(exe, title);
            for (tool, args) in [
                ("Click", json!({"loc": [1, 2]})),
                ("Type", json!({"text": "hi"})),
                ("MultiEdit", json!({"text": "x"})),
                ("MultiSelect", json!({"loc": [1, 2]})),
                ("Shortcut", json!({"shortcut": "ctrl+l"})),
                ("Move", json!({"loc": [1, 2], "drag": true})),
                ("click", json!({"loc": [1, 2]})),
            ] {
                assert!(deny(&tier_override(tool, &args, Some(&f), Policy::Strict)), "{exe} {tool}");
            }
            // Looking is never refused: a snapshot, a screenshot, a scroll, a plain pointer move, waiting.
            for (tool, args) in [("Snapshot", json!({})), ("Screenshot", json!({})), ("Scroll", json!({"direction": "down"})), ("Move", json!({"loc": [1, 2]})), ("Wait", json!({})), ("App", json!({"mode": "switch", "name": "x"}))] {
                assert_eq!(tier_override(tool, &args, Some(&f), Policy::Strict), Override::None, "{exe} {tool}");
            }
        }
        let n = fg("notepad.exe", "Untitled");
        for tool in ["Click", "Type", "Shortcut", "MultiEdit"] {
            assert_eq!(tier_override(tool, &json!({"text": "hi", "shortcut": "ctrl+s", "loc": [1, 2]}), Some(&n), Policy::Strict), Override::None, "a plain app: {tool}");
        }
        assert_eq!(tier_override("Click", &json!({"loc": [1, 2]}), None, Policy::Strict), Override::None, "an unknown window is the guard's question");
    }

    #[test]
    fn a_terminal_is_click_only_and_a_click_is_plain_and_left() {
        let t = fg("powershell.exe", "Windows PowerShell");
        // A plain left click is not refused by the tier (and the tier has no way to answer a question for the guard: it says nothing, and the guard's question stands).
        assert_eq!(tier_override("Click", &json!({"loc": [10, 10]}), Some(&t), Policy::Strict), Override::None);
        assert_eq!(tier_override("Click", &json!({"loc": [10, 10], "button": "left", "clicks": 1}), Some(&t), Policy::Strict), Override::None);
        assert_eq!(tier_override("Click", &json!({"loc": [10, 10], "button": " Left ", "clicks": "1"}), Some(&t), Policy::Strict), Override::None);
        for odd in [json!({"loc": [1, 2], "button": "right"}), json!({"loc": [1, 2], "button": "middle"}), json!({"loc": [1, 2], "clicks": 2}), json!({"loc": [1, 2], "clicks": 3}), json!({"loc": [1, 2], "button": ["left"]}), json!({"loc": [1, 2], "clicks": "2"})] {
            assert!(deny(&tier_override("Click", &odd, Some(&t), Policy::Strict)), "{odd}");
        }
        for (tool, args) in [
            ("Type", json!({"text": "whoami"})),
            ("MultiEdit", json!({"text": "x"})),
            ("Shortcut", json!({"shortcut": "enter"})),
            ("Shortcut", json!({"shortcut": "ctrl+v"})),
            ("MultiSelect", json!({"loc": [1, 2]})),
            ("Move", json!({"loc": [1, 2], "from_loc": [3, 4]})),
        ] {
            let o = tier_override(tool, &args, Some(&t), Policy::Strict);
            assert!(matches!(&o, Override::Deny(why) if why.contains("click-only") || why.contains("plain left click")), "{tool}: {o:?}");
        }
        // The reason names what to use instead: the agent is not left with a dead end.
        assert!(matches!(tier_override("Type", &json!({"text": "dir"}), Some(&t), Policy::Strict), Override::Deny(why) if why.contains("terminal tool")));
        assert!(matches!(tier_override("Click", &json!({"loc": [1, 2]}), Some(&fg("chrome.exe", "x")), Policy::Strict), Override::Deny(why) if why.contains("browser_click")));
    }

    #[test]
    fn the_relaxed_policy_adds_nothing_and_the_setting_is_read_as_written() {
        for (exe, tool, args) in [("chrome.exe", "Type", json!({"text": "x"})), ("powershell.exe", "Type", json!({"text": "x"})), ("regedit.exe", "Click", json!({"loc": [1, 2]}))] {
            assert_eq!(tier_override(tool, &args, Some(&fg(exe, "x")), Policy::Relaxed), Override::None, "{exe}");
        }
        assert_eq!(Policy::parse(None), Policy::Strict);
        assert_eq!(Policy::parse(Some("")), Policy::Strict);
        assert_eq!(Policy::parse(Some("strict")), Policy::Strict);
        assert_eq!(Policy::parse(Some("nonsense")), Policy::Strict, "a setting nobody understands does not loosen anything");
        for relaxed in ["relaxed", "RELAXED", " ask ", "off"] {
            assert_eq!(Policy::parse(Some(relaxed)), Policy::Relaxed, "{relaxed}");
        }
    }
}
