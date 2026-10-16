//! Call-time policy for Windows-MCP (desktop UI automation).
//!
//! Windows-MCP ships shell, file, registry, process, scrape and clipboard tools next to its
//! UI tools. Aqua only wants the UI tools, and it must not rely on the server for that: the
//! server flag `--tools` is a first layer, this guard is the second and is what the agent, the
//! API and the voice path all go through. Every `tools/call` to Windows-MCP is made from
//! [`WindowsGuard::call`]; a source test in `tests/windows_guard_routes.rs` keeps it that way.

use crate::browser::audit::{now_ms, AuditEntry, AuditLog};
use crate::browser::confirm::{class_words, ConfirmKind, ConfirmWhy, ConfirmationBroker, Decision};
use crate::stdio_mcp::{McpTool, StdioMcpClient};
use serde_json::{json, Value};
use std::path::PathBuf;
use super::{critical, session, tiers};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// The only Windows-MCP tools Aqua exposes. Order matches the `--tools` launch flag.
pub const WINDOWS_UI_TOOLS: &[&str] = &[
    "App", "Snapshot", "Screenshot", "DisplayInventory", "Click", "Type", "Scroll", "Move",
    "Shortcut", "Wait", "WaitFor", "MultiSelect", "MultiEdit", "Notification",
];

/// Tools that exist on the server and must never be reachable. Kept for a precise error.
pub const WINDOWS_BLOCKED_TOOLS: &[&str] =
    &["PowerShell", "FileSystem", "Registry", "Process", "Scrape", "Clipboard", "Shell"];

/// Tools that only look at the screen.
const OBSERVE_TOOLS: &[&str] = &[
    "Snapshot", "Screenshot", "DisplayInventory", "Wait", "WaitFor", "Move", "Scroll", "Notification",
];

/// Programs that turn typing into command execution or system administration.
const SHELL_AND_ADMIN_APPS: &[&str] = &[
    "powershell", "pwsh", "cmd", "command prompt", "windows terminal", "wt", "terminal", "conhost",
    "bash", "wsl", "git bash", "mintty", "regedit", "registry editor", "mmc", "services",
    "task manager", "taskmgr", "gpedit", "secpol", "compmgmt", "computer management",
    "device manager", "devmgmt", "control panel", "control", "msconfig", "run", "wscript",
    "cscript", "mshta", "python", "node", "windows powershell ise", "powershell_ise",
    // Start-menu names that are really a shell ("Windows PowerShell", "Developer Command Prompt for VS 2022", ...)
    "anaconda prompt", "git cmd", "ubuntu", "debian", "kali", "opensuse", "developer powershell", "x64 native tools",
];

/// Keyboard shortcuts that open a command surface or bypass the app the agent is driving.
const BLOCKED_SHORTCUTS: &[&str] = &[
    "win+r", "windows+r", "win+x", "windows+x", "ctrl+shift+esc", "ctrl+alt+delete", "ctrl+alt+del",
    "win+s", "windows+s", "win+e", "windows+e", "win+pause", "win+i", "windows+i",
    // The Start menu and search take typed text and run it: Ctrl+Esc, the Windows key alone, and Win+Q.
    "ctrl+esc", "win", "windows", "lwin", "rwin", "super", "meta", "win+q", "windows+q", "win+a", "windows+a",
];

/// One key of a shortcut under the single name this guard knows it by. The shortcut tool takes the Windows key under many names and the keys
/// in any order, so a list of spellings let "rwin+r" open the Run box with no question (and the agent could then type and run anything).
fn canonical_key(key: &str) -> String {
    match key {
        "windows" | "win" | "lwin" | "rwin" | "winleft" | "winright" | "super" | "meta" | "cmd" | "command" => "win",
        "control" | "ctl" | "lctrl" | "rctrl" | "ctrlleft" | "ctrlright" => "ctrl",
        "lalt" | "ralt" | "altleft" | "altright" | "option" => "alt",
        "lshift" | "rshift" | "shiftleft" | "shiftright" => "shift",
        "escape" => "esc",
        "delete" => "del",
        "return" => "enter",
        other => other,
    }
    .to_string()
}

/// The keys of a shortcut, each under its one name, sorted and without repeats ("Ctrl+Shift+Escape" and "shift+control+esc" are the same).
fn canonical_keys(combo: &str) -> Vec<String> {
    let mut keys: Vec<String> = combo.split('+').map(canonical_key).filter(|k| !k.is_empty()).collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Is this a shortcut that opens a command or system surface, under whatever spelling and in whatever order? Anything pressed with the
/// Windows key is (Start, search, Run, the quick-link menu, settings, lock, the clipboard history); so are the Task Manager keys, the security
/// screen and Ctrl+Esc.
fn shortcut_is_blocked(combo: &str) -> bool {
    let keys = canonical_keys(combo);
    if keys.iter().any(|k| k == "win") {
        return true;
    }
    [&["ctrl", "esc"][..], &["ctrl", "esc", "shift"][..], &["alt", "ctrl", "del"][..]].iter().any(|blocked| keys.len() == blocked.len() && keys.iter().zip(blocked.iter()).all(|(a, b)| a == b))
}

const MAX_ARGS_BYTES: usize = 16 * 1024;
const MAX_STRING_CHARS: usize = 4000;
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

/// Who is calling. Voice can look, open an app by name, scroll and type text into the window in front (decision D-14); the agent and the API can act.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    Agent,
    Api,
    Voice,
}

/// The window that will receive the input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Foreground {
    /// Executable file name without directory, lower case (`chrome.exe`).
    pub exe: String,
    pub title: String,
    /// The window's handle (0 when it is not known): two windows of one program are two windows, and a title changes as text is typed.
    pub window: isize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Needs the user's answer; the string is what they are asked.
    Ask(String),
    Deny(DenyKind, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyKind {
    /// The tool exists but Aqua never exposes it (`PERMISSION_DENIED`, stops the run).
    Blocked,
    /// The arguments are malformed or the tool does not exist (`INVALID_ARGUMENTS`).
    Invalid,
}

fn canonical(tool: &str) -> Option<&'static str> {
    WINDOWS_UI_TOOLS.iter().copied().find(|t| t.eq_ignore_ascii_case(tool))
}

fn is_blocked_name(tool: &str) -> bool {
    WINDOWS_BLOCKED_TOOLS.iter().any(|t| t.eq_ignore_ascii_case(tool))
}

/// Keep only the tools the guard would let through, so the model never even sees the rest.
pub fn filter_tools(tools: Vec<McpTool>) -> Vec<McpTool> {
    tools.into_iter().filter(|t| canonical(&t.name).is_some()).collect()
}

fn is_shell_app(name: &str) -> bool {
    let n = name.trim().trim_end_matches(".exe").to_ascii_lowercase();
    // The name may carry words before or after the program ("Windows PowerShell", "Developer Command Prompt for VS"),
    // so a shell name counts when it stands as whole words anywhere in it.
    let padded = format!(" {} ", n.replace(['-', '_', '.'], " "));
    SHELL_AND_ADMIN_APPS.iter().any(|a| n == *a || padded.contains(&format!(" {} ", a.replace('_', " "))))
}

pub(super) fn foreground_is_shell(fg: &Foreground) -> bool {
    let lower = fg.exe.to_ascii_lowercase();
    let exe = lower.trim_end_matches(".exe");
    // Text typed into the Run box (it belongs to explorer.exe; its title is "Run") or into the Start menu's search is run as a command, so
    // they count as shells though neither is a shell program.
    let command_surface = matches!(exe, "searchhost" | "searchapp" | "searchui" | "startmenuexperiencehost") || (exe == "explorer" && fg.title.trim().eq_ignore_ascii_case("run"));
    command_surface
        || ["powershell", "pwsh", "cmd", "windowsterminal", "wt", "conhost", "bash", "wsl", "mintty",
            "regedit", "mmc", "taskmgr", "powershell_ise", "openconsole", "alacritty", "wezterm-gui"]
            .contains(&exe)
}

pub(super) fn foreground_is_browser(fg: &Foreground) -> bool {
    let lower = fg.exe.to_ascii_lowercase();
    let exe = lower.trim_end_matches(".exe");
    ["chrome", "msedge", "firefox", "brave", "opera", "vivaldi", "iexplore"].contains(&exe)
}

/// A `Type` call that is only text: no place to click first (`loc`), no Enter, no clearing of the field.
/// The most characters a question can show of what would be typed into a shell.
const MAX_SHOWN_TYPED_INTO_A_SHELL: usize = 1000;

fn voice_types_only_text(args: &Value) -> bool {
    // A line break in the text is typed as the Enter key: the voice does not press Enter, so that is not "only text".
    let text_is_there = args.get("text").and_then(|v| v.as_str()).is_some_and(|t| !t.trim().is_empty() && !t.contains(['\n', '\r']));
    let nothing_else = args.as_object().is_some_and(|o| o.iter().all(|(k, v)| k == "text" || v.is_null() || *v == Value::Bool(false)));
    text_is_there && nothing_else
}

fn app_mode_is_launch_or_switch(args: &Value) -> bool {
    match args.get("mode").and_then(|v| v.as_str()) {
        None => true,
        Some(m) => matches!(m.trim().to_ascii_lowercase().as_str(), "launch" | "switch"),
    }
}

fn largest_string(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        Value::Array(a) => a.iter().map(largest_string).max().unwrap_or(0),
        Value::Object(o) => o.values().map(largest_string).max().unwrap_or(0),
        _ => 0,
    }
}

/// Pure policy: no I/O, so every rule is unit-testable. `fg` is `None` when the foreground
/// window cannot be read (non-Windows, or the API failed); rules that need it then ask.
pub fn evaluate(tool: &str, args: &Value, caller: Caller, fg: Option<&Foreground>) -> Verdict {
    evaluate_with(tool, args, caller, fg, crate::full_access::is_on())
}

/// `evaluate` with the full-access switch given. Under it the questions about ordinary input go away (typing into a browser
/// window, a Ctrl+Enter, not knowing which window is in front). Every refusal stays, and so does the question before input goes
/// to a shell or an administration console; input to Aqua's own window is still refused because that window holds the prompts.
pub fn evaluate_with(tool: &str, args: &Value, caller: Caller, fg: Option<&Foreground>, full_access: bool) -> Verdict {
    let Some(name) = canonical(tool) else {
        return if is_blocked_name(tool) {
            Verdict::Deny(
                DenyKind::Blocked,
                format!("{tool} is not available: Aqua does not give the agent a shell, file, registry, process, scrape or clipboard tool."),
            )
        } else {
            Verdict::Deny(DenyKind::Invalid, format!("unknown tool {tool}"))
        };
    };

    if !(args.is_object() || args.is_null()) {
        return Verdict::Deny(DenyKind::Invalid, "arguments must be a JSON object".into());
    }
    let size = serde_json::to_string(args).map(|s| s.len()).unwrap_or(usize::MAX);
    if size > MAX_ARGS_BYTES || largest_string(args) > MAX_STRING_CHARS {
        return Verdict::Deny(DenyKind::Invalid, format!("{name}: arguments are too large"));
    }

    if is_observe_call(name, args) {
        return Verdict::Allow;
    }
    // Voice (and typed orb requests) may look at the screen, start or switch to an app by name ("open Notepad"), scroll, and type text into the window in front
    // (decision D-14 of 2026-10-07). Clicking, key presses, Enter and sending stay with the app, and the App rules below still apply (no shells, no arguments).
    if caller == Caller::Voice {
        let allowed = (name == "App" && app_mode_is_launch_or_switch(args)) || name == "Scroll" || (name == "Type" && voice_types_only_text(args));
        if !allowed {
            return Verdict::Deny(
                DenyKind::Blocked,
                format!("{name} cannot be started by voice; voice may look at the screen, open apps by name, scroll and type text into the window in front."),
            );
        }
        if name == "Type" {
            // Typing by voice goes only where the agent's own rules let it through with no question: not a shell or an administration console, not Aqua's own
            // window, not a browser (a page can post what is typed), and only when the window in front is known. A spoken request is never asked: a television
            // can say the wake word, so the full-access switch does not widen this either.
            return match evaluate_with(tool, args, Caller::Agent, fg, false) {
                Verdict::Allow => Verdict::Allow,
                Verdict::Ask(why) => Verdict::Deny(DenyKind::Blocked, format!("Type by voice: {why}, which needs a yes in the app, so it is not done by voice.")),
                deny => deny,
            };
        }
    }

    match name {
        "App" => {
            // `executable`, `args` and `cwd` launch an arbitrary program with arbitrary arguments.
            for forbidden in ["executable", "args", "cwd"] {
                if args.get(forbidden).is_some_and(|v| !v.is_null()) {
                    return Verdict::Deny(
                        DenyKind::Blocked,
                        format!("App: '{forbidden}' is not allowed; launch apps by name only."),
                    );
                }
            }
            let app = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if is_shell_app(app) {
                return Verdict::Deny(
                    DenyKind::Blocked,
                    format!("App: '{app}' is a shell or administration tool and cannot be started by the agent."),
                );
            }
            Verdict::Allow
        }
        "Shortcut" => {
            let combo = args
                .get("shortcut")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .replace(' ', "");
            if BLOCKED_SHORTCUTS.contains(&combo.as_str()) || shortcut_is_blocked(&combo) {
                return Verdict::Deny(
                    DenyKind::Blocked,
                    format!("Shortcut {combo} opens a command or system surface and is not allowed."),
                );
            }
            if let Some(v) = input_target_verdict(fg, &format!("press {combo}"), full_access) {
                return v;
            }
            if canonical_keys(&combo) == ["ctrl", "enter"] && !full_access {
                return Verdict::Ask(format!("Press Ctrl+Enter (often sends a message or form) in {}", describe(fg)));
            }
            Verdict::Allow
        }
        "Type" | "MultiEdit" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let action = if name == "Type" && fg.is_some_and(foreground_is_shell) {
                // What would be run is what the person is asked about, so the prompt says all of it, on one line. Text too long to show whole is not typed into a shell.
                match aqua_tool_host::shown_whole(text, MAX_SHOWN_TYPED_INTO_A_SHELL) {
                    Ok(shown) => format!("type {} characters (\"{shown}\")", text.chars().count()),
                    Err(n) => {
                        return Verdict::Deny(
                            DenyKind::Invalid,
                            format!("{n} characters are too many to type into a shell: a question shows {MAX_SHOWN_TYPED_INTO_A_SHELL} at most, and it is not typed on a question that would hide part of it."),
                        )
                    }
                }
            } else if name == "Type" {
                format!("type {} characters", text.chars().count())
            } else {
                "fill several fields".into()
            };
            if let Some(v) = input_target_verdict(fg, &action, full_access) {
                return v;
            }
            if fg.is_some_and(foreground_is_browser) && !full_access {
                return Verdict::Ask(format!("Type into the browser window ({}): {action}", describe(fg)));
            }
            // Typing and pressing Enter in one call is a send like Ctrl+Enter: in a chat or a form it posts what was typed.
            if name == "Type" && presses_enter(args) && !full_access {
                return Verdict::Ask(format!("{action} and press Enter (often sends a message or form) in {}", describe(fg)));
            }
            Verdict::Allow
        }
        "Click" | "MultiSelect" | "Move" => {
            // `Move` only gets here as a drag.
            let action = if name == "Move" { "drag" } else { "click" };
            if let Some(v) = input_target_verdict(fg, action, full_access) {
                return v;
            }
            Verdict::Allow
        }
        _ => Verdict::Allow,
    }
}

/// What the guard decides now: the rules above, then the tier of the window in front (card R6-34). A refusal of the rules stands whatever the tier says; the tier only adds
/// refusals (a browser or a command window is view-only, a terminal click-only) and never takes a question away: what the rules ask, they still ask.
pub fn evaluate_tiered(tool: &str, args: &Value, caller: Caller, fg: Option<&Foreground>, full_access: bool, policy: tiers::Policy) -> Verdict {
    let base = evaluate_with(tool, args, caller, fg, full_access);
    if matches!(base, Verdict::Deny(..)) {
        return base;
    }
    match tiers::tier_override(tool, args, fg, policy) {
        tiers::Override::None => base,
        tiers::Override::Deny(why) => Verdict::Deny(DenyKind::Blocked, why),
    }
}

/// [`evaluate_tiered`] with the full-access switch and the tier setting of this process.
pub fn evaluate_now(tool: &str, args: &Value, caller: Caller, fg: Option<&Foreground>) -> Verdict {
    evaluate_tiered(tool, args, caller, fg, crate::full_access::is_on(), tiers::Policy::from_env())
}

/// Input aimed at a shell, an admin console or Aqua's own window is never sent silently.
fn input_target_verdict(fg: Option<&Foreground>, action: &str, full_access: bool) -> Option<Verdict> {
    let Some(fg) = fg else {
        if full_access {
            return None;
        }
        return Some(Verdict::Ask(format!("Aqua could not tell which window is in front; {action} anyway?")));
    };
    if fg.title.to_ascii_lowercase().contains("aqua") && !foreground_is_shell(fg) {
        return Some(Verdict::Deny(
            DenyKind::Blocked,
            format!("Refusing to {action} in Aqua's own window ({}): it holds the approval prompts.", describe(Some(fg))),
        ));
    }
    if foreground_is_shell(fg) {
        return Some(Verdict::Ask(format!("{action} in a shell or admin console ({})", describe(Some(fg)))));
    }
    None
}

pub(super) fn describe(fg: Option<&Foreground>) -> String {
    match fg {
        Some(f) if !f.title.is_empty() => format!("{} — \"{}\"", f.exe, f.title),
        Some(f) => f.exe.clone(),
        None => "unknown window".into(),
    }
}

/// What a question of the guard shows besides the action, as the host's questions do (card R6-08, T6-08.8): the classes of the call, what could go wrong, the rule that made it ask
/// (the guard's own sentence) and the window it is about. The guard does not know what the run has read or holds, so it says nothing of that; the question the host puts for the
/// same call says it.
pub(super) fn question_explained(reason: &str, fg: Option<&Foreground>) -> (Vec<String>, ConfirmWhy) {
    let classes = super::provider::WindowsUiProvider::question_classes(reason);
    let why = aqua_tool_host::explain(&classes, &describe(fg), reason, false, false, false);
    (class_words(&classes), why.into())
}

/// Foreground window's executable and title (Windows only).
#[cfg(windows)]
pub fn foreground_window() -> Option<Foreground> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };
    // SAFETY: buffers are sized to the lengths passed; handles are closed before returning.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut title = [0u16; 512];
        let n = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32).max(0) as usize;
        let title = String::from_utf16_lossy(&title[..n.min(512)]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return Some(Foreground { exe: String::new(), title, window: hwnd as isize });
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        let exe = if ok != 0 {
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            path.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase()
        } else {
            String::new()
        };
        Some(Foreground { exe, title, window: hwnd as isize })
    }
}

#[cfg(not(windows))]
pub fn foreground_window() -> Option<Foreground> {
    None
}

/// Text form of an error the agent loop recognises (`ERROR [CODE] tool: message`).
fn error_text(code: &str, tool: &str, message: &str) -> String {
    format!("ERROR [{code}] {tool}: {message}")
}

/// Replace typed text with its length so credentials never reach the audit log.
fn audit_args(tool: &str, args: &Value) -> Value {
    let mut a = args.clone();
    let redact = |v: &mut Value| {
        if let Some(s) = v.as_str() {
            *v = Value::String(format!("<redacted {} chars>", s.chars().count()));
        }
    };
    if tool.eq_ignore_ascii_case("Type") {
        if let Some(t) = a.get_mut("text") {
            redact(t);
        }
    }
    a
}

/// The lease on the desktop a guard uses: the one of the process (the Esc key reads it) for the global guard, and one of its own for a guard made for a test.
enum DeskRef {
    Global,
    Own(session::Desk),
}

impl DeskRef {
    fn get(&self) -> &session::Desk {
        match self {
            DeskRef::Global => session::global(),
            DeskRef::Own(d) => d,
        }
    }
}

/// What the last snapshot of a window showed, for the sign-in and payment steps: the process's, which the guard that serves the routes and the host's desktop tools both read
/// and write (see [`Screen`]).
static GLOBAL_VIEW: Mutex<Option<critical::View>> = Mutex::new(None);

enum ViewRef {
    Global,
    Own(Mutex<Option<critical::View>>),
}

impl ViewRef {
    fn cell(&self) -> &Mutex<Option<critical::View>> {
        match self {
            ViewRef::Global => &GLOBAL_VIEW,
            ViewRef::Own(m) => m,
        }
    }
}

/// The desktop as the routes to it share it (card R6-08): the lease that lets one session act on the screen at a time, and what the last snapshot of a window showed. The guard
/// (the old route) and the host's desktop tools (the one door) both hold the process's, so a snapshot taken through one is seen through the other, and a session that has the
/// desktop through one is refused through the other. A test makes one of its own.
pub struct Screen {
    desk: DeskRef,
    view: ViewRef,
}

impl Screen {
    /// The process's desktop (the lease the Esc key reads, and the last snapshot).
    pub fn global() -> Self {
        Self { desk: DeskRef::Global, view: ViewRef::Global }
    }

    /// A desktop of its own, for a test.
    pub fn own() -> Self {
        Self { desk: DeskRef::Own(session::Desk::new()), view: ViewRef::Own(Mutex::new(None)) }
    }

    pub fn desk(&self) -> &session::Desk {
        self.desk.get()
    }

    /// Is this window at a sign-in or payment step: by its title, or by what the last snapshot of the same window showed a short while ago?
    pub fn step_in(&self, fg: &Foreground) -> Option<critical::Critical> {
        let view = self.view.cell().lock().unwrap_or_else(PoisonError::into_inner).clone();
        critical::critical_for(fg, view.as_ref(), now_ms())
    }

    /// A snapshot of `fg` came back with this text.
    pub fn note_snapshot(&self, fg: &Foreground, text: &str) {
        *self.view.cell().lock().unwrap_or_else(PoisonError::into_inner) = Some(critical::View::of(fg, text, now_ms()));
    }
}

pub struct WindowsGuard {
    audit: AuditLog,
    screen: Screen,
}

static GLOBAL: OnceLock<WindowsGuard> = OnceLock::new();

impl WindowsGuard {
    pub fn new(audit_path: PathBuf) -> Self {
        Self::build(audit_path, Screen::own())
    }

    fn build(audit_path: PathBuf, screen: Screen) -> Self {
        if let Some(dir) = audit_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Self { audit: AuditLog::new(audit_path), screen }
    }

    /// Choose where the process-wide guard writes its audit records. Call once at startup, before `global()`;
    /// without it the log goes to `data/tools/audit.jsonl` relative to the current folder, which is only right
    /// when the program was started from the folder that holds `data`.
    pub fn init_global(audit_path: PathBuf) {
        let _ = GLOBAL.set(WindowsGuard::build(audit_path, Screen::global()));
    }

    /// The process-wide guard. Audit records go to the path given to `init_global`.
    pub fn global() -> &'static WindowsGuard {
        GLOBAL.get_or_init(|| WindowsGuard::build(PathBuf::from("data").join("tools").join("audit.jsonl"), Screen::global()))
    }

    fn record(&self, tool: &str, args: &Value, caller: Caller, decision: &str, ok: bool, code: Option<&str>, started: u64) {
        self.audit.append(&AuditEntry {
            ts_unix_ms: now_ms(),
            backend: caller_backend(caller),
            tool,
            args: audit_args(tool, args),
            url_before: None,
            url_after: None,
            decision,
            ok,
            error_code: code,
            duration_ms: now_ms().saturating_sub(started),
        });
    }

    /// The single way to call a Windows-MCP tool. Returns the text the agent (or API caller)
    /// reads: the tool's observation, or `ERROR [CODE] tool: message`.
    pub fn call(
        &self,
        client: &Mutex<StdioMcpClient>,
        broker: Option<&ConfirmationBroker>,
        tool: &str,
        args: Value,
        caller: Caller,
    ) -> String {
        self.call_at(foreground_window(), client, broker, tool, args, caller)
    }

    /// [`WindowsGuard::call`] with the window in front given, so that every refusal and question that comes before the server can be tested without one.
    pub fn call_at(
        &self,
        fg: Option<Foreground>,
        client: &Mutex<StdioMcpClient>,
        broker: Option<&ConfirmationBroker>,
        tool: &str,
        args: Value,
        caller: Caller,
    ) -> String {
        let started = now_ms();
        let name = canonical(tool).unwrap_or(tool);
        let owner = session::current_owner().unwrap_or_else(|| caller_backend(caller).to_string());
        let mut verdict = evaluate_now(tool, &args, caller, fg.as_ref());
        // A sign-in or payment step is asked about whatever the settings are, and never by voice (card R6-34).
        if let Some(f) = fg.as_ref() {
            let step = self.screen.step_in(f);
            verdict = critical::critical_verdict(verdict, name, &args, caller, fg.as_ref(), step.as_ref());
        }
        // One session at a time: a call that acts on the screen while another session has it is refused before anyone is asked; a call that only looks keeps its own lease alive.
        let refused = matches!(verdict, Verdict::Deny(..));
        let acts = !refused && !is_observe_call(name, &args);
        if acts {
            if let Err(why) = self.screen.desk().check_at(&owner, Instant::now()) {
                self.record(name, &args, caller, "blocked", false, Some("PERMISSION_DENIED"), started);
                return error_text("PERMISSION_DENIED", name, &why);
            }
        } else if !refused {
            self.screen.desk().touch_at(&owner, Instant::now());
        }
        let decision = match verdict {
            Verdict::Allow => "allowed",
            Verdict::Ask(reason) => {
                let (answer, _) = match broker {
                    Some(b) => {
                        // The question carries what the host's carries (card R6-08, T6-08.8): what could go wrong, the rule that made it ask, and the window it is about.
                        let (classes, why) = question_explained(&reason, fg.as_ref());
                        let (_, d) = b.request_explained(ConfirmKind::Tool, name, reason.clone(), None, None, true, classes, why, CONFIRM_TIMEOUT);
                        (d, ())
                    }
                    None => (Decision::Deny, ()),
                };
                if answer == Decision::Deny {
                    self.record(name, &args, caller, "denied", false, Some("CONFIRMATION_DENIED"), started);
                    return error_text(
                        "CONFIRMATION_DENIED",
                        name,
                        &format!("the user did not approve: {reason}. Do not retry another way."),
                    );
                }
                // The person may have taken a minute to answer. If another program is in front now, the "yes" was for a different window: nothing is sent.
                let after = foreground_window();
                if input_target_changed(fg.as_ref(), after.as_ref()) {
                    self.record(name, &args, caller, "denied", false, Some("CONFIRMATION_STALE"), started);
                    return error_text(
                        "CONFIRMATION_DENIED",
                        name,
                        &format!("the window in front changed while you were being asked ({} is now in front): nothing was done. Ask again if you still want it.", describe(after.as_ref())),
                    );
                }
                // And the new window is judged as any other: it may be one that is refused outright (Aqua's own).
                if let Verdict::Deny(kind, reason) = evaluate_now(tool, &args, caller, after.as_ref()) {
                    let code = if kind == DenyKind::Blocked { "PERMISSION_DENIED" } else { "INVALID_ARGUMENTS" };
                    self.record(name, &args, caller, "blocked", false, Some(code), started);
                    return error_text(code, name, &reason);
                }
                "confirmed"
            }
            Verdict::Deny(kind, reason) => {
                let code = match kind {
                    DenyKind::Blocked => "PERMISSION_DENIED",
                    DenyKind::Invalid => "INVALID_ARGUMENTS",
                };
                self.record(name, &args, caller, "blocked", false, Some(code), started);
                return error_text(code, name, &reason);
            }
        };

        if acts {
            // The lease is taken only now, after the question (a person may take a minute, and a "no" must not keep the desktop from another session).
            if let Err(why) = self.screen.desk().claim_at(&owner, Instant::now()) {
                self.record(name, &args, caller, "blocked", false, Some("PERMISSION_DENIED"), started);
                return error_text("PERMISSION_DENIED", name, &why);
            }
        }
        match forward(client, name, &args) {
            Ok(text) => {
                if matches!(name, "Snapshot" | "WaitFor") {
                    if let Some(f) = fg.as_ref() {
                        self.screen.note_snapshot(f, &text);
                    }
                }
                self.record(name, &args, caller, decision, true, None, started);
                text
            }
            Err(ForwardError::Unavailable(e)) => {
                self.record(name, &args, caller, decision, false, Some("UNAVAILABLE"), started);
                error_text("UNAVAILABLE", name, &format!("Windows-MCP is not running: {e}"))
            }
            Err(ForwardError::Backend(e)) => {
                self.record(name, &args, caller, decision, false, Some("BACKEND_ERROR"), started);
                error_text("BACKEND_ERROR", name, &e)
            }
        }
    }
}

#[derive(Debug)]
pub enum ForwardError {
    Unavailable(String),
    Backend(String),
}

/// Send an already-approved call to the server. This and [`WindowsGuard::call`] are the only
/// places in Aqua that issue a Windows-MCP `tools/call`; callers must have passed [`evaluate`].
pub fn forward(client: &Mutex<StdioMcpClient>, name: &str, args: &Value) -> Result<String, ForwardError> {
    let mut c = client.lock().map_err(|_| ForwardError::Backend("Windows-MCP client lock poisoned".into()))?;
    if !c.is_initialized() {
        c.start().map_err(|e| ForwardError::Unavailable(format!("{e:#}")))?;
    }
    c.call_tool(name, args.clone()).map(|r| observation_text(&r)).map_err(|e| ForwardError::Backend(format!("{e:#}")))
}

/// Did the window that would get the input change while the person was being asked? A different program is a different question, and so is a second window of the same
/// program (another chat, another mail): the handle says which, because a title changes as text is typed. A window that could not be read when the question was put is
/// not a change (the person said yes to "I could not tell which window"); one that can no longer be read after a yes is.
pub(super) fn input_target_changed(before: Option<&Foreground>, after: Option<&Foreground>) -> bool {
    match (before, after) {
        (Some(b), Some(a)) => !b.exe.eq_ignore_ascii_case(&a.exe) || (b.window != 0 && a.window != 0 && b.window != a.window),
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Does this `Type` call press Enter? Said either way: the flag, which the server accepts as a boolean or as text or a number (so `"true"` and `1` count), and a line
/// break in the text, which the server types as the Enter key. Anything that is not clearly "no" counts, because a guard that reads a call more narrowly than the server
/// does decides about a different call.
pub(super) fn presses_enter(args: &Value) -> bool {
    let flag = match args.get("press_enter") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64().is_none_or(|f| f != 0.0),
        Some(Value::String(s)) => !matches!(s.trim().to_ascii_lowercase().as_str(), "" | "false" | "0" | "no" | "off" | "none" | "null"),
        Some(_) => true,
    };
    flag || args.get("text").and_then(|v| v.as_str()).is_some_and(|t| t.contains(['\n', '\r']))
}

/// What the guard would decide right now, reading the foreground window.
pub fn classify_now(tool: &str, args: &Value, caller: Caller) -> Verdict {
    evaluate_now(tool, args, caller, foreground_window().as_ref())
}

pub fn is_observe_tool(name: &str) -> bool {
    OBSERVE_TOOLS.contains(&name)
}

/// `Move` with `drag` set presses the mouse button down, moves and lets go: it picks something up and drops it somewhere, which
/// changes things (a file moved, an attachment sent). Any other `Move` only repositions the pointer.
pub(super) fn is_drag(name: &str, args: &Value) -> bool {
    if name != "Move" {
        return false;
    }
    let truthy = |v: &Value| match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
        _ => false,
    };
    // `from_loc` only has a meaning for a drag, so a call that gives one is treated as one.
    args.get("drag").is_some_and(truthy) || args.get("from_loc").is_some_and(|v| !v.is_null())
}

/// Whether this call only looks at the screen: an observe tool, and not a drag.
pub fn is_observe_call(name: &str, args: &Value) -> bool {
    is_observe_tool(name) && !is_drag(name, args)
}

pub fn shell_or_admin_reason(reason: &str) -> bool {
    reason.contains("shell or admin")
}

pub(super) fn caller_backend(caller: Caller) -> &'static str {
    match caller {
        Caller::Agent => "windows:agent",
        Caller::Api => "windows:api",
        Caller::Voice => "windows:voice",
    }
}

/// Flatten an MCP `tools/call` result to text, dropping image payloads.
pub fn observation_text(result: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = result.get("content").and_then(|c| c.as_array()) {
        for item in items {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                        parts.push(t.to_string());
                    }
                }
                Some("image") => parts.push("[image omitted]".to_string()),
                _ => {}
            }
        }
    }
    if parts.is_empty() {
        return json!(result).to_string();
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fg(exe: &str, title: &str) -> Foreground {
        Foreground { exe: exe.into(), title: title.into(), window: 0 }
    }

    fn is_blocked(v: &Verdict) -> bool {
        matches!(v, Verdict::Deny(DenyKind::Blocked, _))
    }

    #[test]
    fn blocked_tools_are_denied_with_permission_denied() {
        for t in ["PowerShell", "FileSystem", "Registry", "Process", "Scrape", "Clipboard", "powershell"] {
            let v = evaluate(t, &json!({"command": "whoami"}), Caller::Agent, None);
            assert!(is_blocked(&v), "{t} must be blocked, got {v:?}");
        }
    }

    #[test]
    fn unknown_tool_is_invalid_not_forwarded() {
        let v = evaluate("Frobnicate", &json!({}), Caller::Agent, None);
        assert!(matches!(v, Verdict::Deny(DenyKind::Invalid, _)));
    }

    #[test]
    fn allowlist_is_exactly_the_fourteen_ui_tools() {
        assert_eq!(WINDOWS_UI_TOOLS.len(), 14);
        for blocked in WINDOWS_BLOCKED_TOOLS {
            assert!(!WINDOWS_UI_TOOLS.iter().any(|t| t.eq_ignore_ascii_case(blocked)));
        }
    }

    #[test]
    fn filter_tools_hides_everything_else() {
        let mk = |n: &str| McpTool { name: n.into(), description: String::new(), input_schema: json!({}), source: None };
        let kept = filter_tools(vec![mk("Click"), mk("PowerShell"), mk("Registry"), mk("Snapshot"), mk("Clipboard")]);
        let names: Vec<_> = kept.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Click", "Snapshot"]);
    }

    #[test]
    fn app_cannot_launch_arbitrary_executables() {
        let v = evaluate("App", &json!({"mode": "launch", "executable": "powershell.exe", "args": "-c calc"}), Caller::Agent, None);
        assert!(is_blocked(&v));
        let v = evaluate("App", &json!({"mode": "launch", "name": "Notepad", "cwd": "C:\\"}), Caller::Agent, None);
        assert!(is_blocked(&v));
    }

    #[test]
    fn app_cannot_launch_shells_or_admin_tools() {
        for n in ["PowerShell", "cmd", "Windows Terminal", "Registry Editor", "regedit.exe", "Task Manager",
                  "Windows PowerShell", "Developer Command Prompt for VS 2022", "Anaconda Prompt", "Git CMD", "Ubuntu", "windows-powershell"] {
            let v = evaluate("App", &json!({"mode": "launch", "name": n}), Caller::Agent, None);
            assert!(is_blocked(&v), "{n}");
        }
        assert_eq!(evaluate("App", &json!({"mode": "launch", "name": "Notepad"}), Caller::Agent, None), Verdict::Allow);
        assert_eq!(evaluate("App", &json!({"mode": "launch", "name": "Chrome"}), Caller::Agent, None), Verdict::Allow);
        for n in ["Calculator", "Microsoft Edge", "Visual Studio Code", "Runtime Settings", "Controller Setup"] {
            assert_eq!(evaluate("App", &json!({"mode": "launch", "name": n}), Caller::Agent, None), Verdict::Allow, "{n} is not a shell");
        }
    }

    #[test]
    fn run_dialog_and_task_manager_shortcuts_are_blocked() {
        let f = fg("notepad.exe", "Untitled - Notepad");
        for s in ["win+r", "Win + R", "ctrl+shift+esc", "win+x", "ctrl+esc", "win", "Win+Q", "windows"] {
            let v = evaluate("Shortcut", &json!({"shortcut": s}), Caller::Agent, Some(&f));
            assert!(is_blocked(&v), "{s}");
        }
        assert_eq!(evaluate("Shortcut", &json!({"shortcut": "ctrl+s"}), Caller::Agent, Some(&f)), Verdict::Allow);
    }

    /// Found in a review: only the spellings "win" and "windows" were listed, so "RWin+r" (and "LWin+s", "super+e", "winleft+r", "r+win") reached the
    /// Run box and the Start menu with no question, and the agent's own `win.type` could then type and run anything.
    #[test]
    fn the_windows_key_is_blocked_under_every_name_and_in_every_order() {
        let f = fg("notepad.exe", "Untitled - Notepad");
        for s in [
            "RWin+r", "lwin+r", "LWin + S", "super+e", "meta+r", "winleft+r", "winright+x", "r+win", "Win+Shift+S", "windows+d", "cmd+r", "win+l", "win+v", "win+tab",
            "control+shift+escape", "shift+ctrl+esc", "esc+ctrl", "ctrl+alt+delete", "alt+ctrl+del", "CTRL + ALT + DEL", "lctrl+esc",
        ] {
            let v = evaluate("Shortcut", &json!({"shortcut": s}), Caller::Agent, Some(&f));
            assert!(is_blocked(&v), "{s}");
            assert!(is_blocked(&evaluate_with("Shortcut", &json!({"shortcut": s}), Caller::Agent, Some(&f), true)), "{s}: full access does not change it");
        }
        for s in ["ctrl+s", "ctrl+shift+s", "alt+tab", "ctrl+c", "ctrl+v", "enter", "f5", "alt+f4", "ctrl+alt+t", "shift+delete", "escape", "ctrl+z", "ctrl+shift+n"] {
            let v = evaluate("Shortcut", &json!({"shortcut": s}), Caller::Agent, Some(&f));
            assert_eq!(v, Verdict::Allow, "{s} is an ordinary shortcut");
        }
        // The question about Ctrl+Enter is asked under its other spellings too.
        for s in ["control+return", "enter+ctrl", "Ctrl + Enter"] {
            assert!(matches!(evaluate("Shortcut", &json!({"shortcut": s}), Caller::Agent, Some(&f)), Verdict::Ask(_)), "{s}");
        }
    }

    /// Found in a review: the Run box is a window of explorer.exe, which is not a shell, so text typed into it (and into the Start menu's search)
    /// was sent without a question.
    #[test]
    fn typing_into_the_run_box_or_the_start_search_asks_and_shows_the_text() {
        for (exe, title) in [("explorer.exe", "Run"), ("explorer.exe", "run"), ("SearchHost.exe", "Search"), ("StartMenuExperienceHost.exe", "Start"), ("SearchApp.exe", "Search")] {
            let f = fg(exe, title);
            match evaluate("Type", &json!({"text": "cmd /c whoami"}), Caller::Agent, Some(&f)) {
                Verdict::Ask(q) => assert!(q.contains("cmd /c whoami"), "the prompt must show what would be typed: {q}"),
                other => panic!("{exe} {title}: {other:?}"),
            }
            assert!(matches!(evaluate_with("Type", &json!({"text": "x"}), Caller::Agent, Some(&f), true), Verdict::Ask(_)), "{exe}: full access still asks");
        }
        // An ordinary Explorer window (a folder) is not a command surface.
        let folder = fg("explorer.exe", "Downloads");
        assert!(!matches!(evaluate("Type", &json!({"text": "notes"}), Caller::Agent, Some(&folder)), Verdict::Ask(q) if q.contains("shell")));
    }

    #[test]
    fn full_access_removes_the_questions_about_ordinary_input_and_nothing_else() {
        let full = |tool: &str, args: Value, f: Option<&Foreground>| evaluate_with(tool, &args, Caller::Agent, f, true);
        let browser = fg("chrome.exe", "Docs - Google Chrome");
        // Asked before, allowed now.
        assert_eq!(full("Type", json!({"text": "hi"}), Some(&browser)), Verdict::Allow);
        assert_eq!(full("Shortcut", json!({"shortcut": "ctrl+enter"}), Some(&fg("notepad.exe", "x"))), Verdict::Allow);
        assert_eq!(full("Click", json!({"loc": [1, 2]}), None), Verdict::Allow, "no foreground window to read");
        // Still asked: input aimed at a shell or an administration console.
        assert!(matches!(full("Type", json!({"text": "whoami"}), Some(&fg("powershell.exe", "Windows PowerShell"))), Verdict::Ask(_)));
        // Still refused: Aqua's own window, shells as apps, blocked tools and shortcuts, voice.
        assert!(matches!(full("Type", json!({"text": "approve"}), Some(&fg("aqua-desktop.exe", "Aqua Assistant"))), Verdict::Deny(DenyKind::Blocked, _)));
        assert!(matches!(full("App", json!({"mode": "launch", "name": "PowerShell"}), None), Verdict::Deny(DenyKind::Blocked, _)));
        assert!(matches!(full("Shortcut", json!({"shortcut": "win+r"}), None), Verdict::Deny(DenyKind::Blocked, _)));
        assert!(matches!(full("Shell", json!({"command": "dir"}), None), Verdict::Deny(DenyKind::Blocked, _)));
        assert!(matches!(evaluate_with("Click", &json!({"loc": [1, 2]}), Caller::Voice, None, true), Verdict::Deny(DenyKind::Blocked, _)), "voice still cannot click");
        // ... and may type text into the window in front, scroll, and nothing else that acts.
        let notepad = Foreground { exe: "notepad.exe".into(), title: "Untitled - Notepad".into(), window: 0 };
        let voice = |tool: &str, args: Value, f: Option<&Foreground>, full: bool| evaluate_with(tool, &args, Caller::Voice, f, full);
        for full in [false, true] {
            assert_eq!(voice("Type", json!({"text": "hello there"}), Some(&notepad), full), Verdict::Allow, "text into the window in front (full access {full})");
            assert_eq!(voice("Scroll", json!({"direction": "down"}), Some(&notepad), full), Verdict::Allow);
            for denied in [
                voice("Type", json!({"text": "hello", "press_enter": true}), Some(&notepad), full),
                voice("Type", json!({"text": "hello", "loc": [10, 10]}), Some(&notepad), full),
                voice("Type", json!({"text": "hello", "clear": true}), Some(&notepad), full),
                voice("Type", json!({"text": "  "}), Some(&notepad), full),
                voice("Type", json!({}), Some(&notepad), full),
                voice("Shortcut", json!({"shortcut": "ctrl+enter"}), Some(&notepad), full),
                voice("Shortcut", json!({"shortcut": "enter"}), Some(&notepad), full),
                voice("Click", json!({"loc": [1, 2]}), Some(&notepad), full),
                voice("MultiEdit", json!({"text": "x"}), Some(&notepad), full),
            ] {
                assert!(matches!(denied, Verdict::Deny(DenyKind::Blocked, _)), "{denied:?} (full access {full})");
            }
            // Where the agent would be asked, the voice is refused: a browser, a shell, Aqua's own window, a window nobody could name.
            for (fg, what) in [
                (Some(Foreground { exe: "chrome.exe".into(), title: "Facebook".into(), window: 0 }), "a browser"),
                (Some(Foreground { exe: "powershell.exe".into(), title: "Windows PowerShell".into(), window: 0 }), "a shell"),
                (Some(Foreground { exe: "aqua-desktop.exe".into(), title: "Aqua Assistant".into(), window: 0 }), "Aqua's own window"),
                (None, "an unknown window"),
            ] {
                let v = voice("Type", json!({"text": "hello"}), fg.as_ref(), full);
                assert!(matches!(v, Verdict::Deny(DenyKind::Blocked, _)), "{what}: {v:?} (full access {full})");
            }
        }
        // The same calls without the switch are unchanged.
        assert!(matches!(evaluate_with("Type", &json!({"text": "hi"}), Caller::Agent, Some(&browser), false), Verdict::Ask(_)));
    }

    #[test]
    fn ctrl_enter_asks() {
        let f = fg("notepad.exe", "x");
        assert!(matches!(evaluate("Shortcut", &json!({"shortcut": "ctrl+enter"}), Caller::Agent, Some(&f)), Verdict::Ask(_)));
    }

    #[test]
    fn typing_into_a_shell_asks_and_into_aqua_is_denied() {
        let shell = fg("powershell.exe", "Windows PowerShell");
        match evaluate("Type", &json!({"text": "whoami /all"}), Caller::Agent, Some(&shell)) {
            Verdict::Ask(q) => assert!(q.contains("whoami /all"), "the prompt must show what would be typed: {q}"),
            other => panic!("{other:?}"),
        }
        // Text typed into a browser can be a password, so that prompt does not repeat it.
        let b = fg("chrome.exe", "Sign in");
        match evaluate("Type", &json!({"text": "hunter2-secret"}), Caller::Agent, Some(&b)) {
            Verdict::Ask(q) => assert!(!q.contains("hunter2-secret"), "{q}"),
            other => panic!("{other:?}"),
        }
        let aqua = fg("msedge.exe", "Aqua Admin - Microsoft Edge");
        assert!(is_blocked(&evaluate("Type", &json!({"text": "y"}), Caller::Agent, Some(&aqua))));
        assert!(is_blocked(&evaluate("Click", &json!({"loc": [1, 2]}), Caller::Agent, Some(&aqua))));
    }

    #[test]
    fn typing_into_a_browser_asks_and_into_a_plain_app_is_allowed() {
        let b = fg("chrome.exe", "Inbox - Gmail");
        assert!(matches!(evaluate("Type", &json!({"text": "hi"}), Caller::Agent, Some(&b)), Verdict::Ask(_)));
        let n = fg("notepad.exe", "Untitled");
        assert_eq!(evaluate("Type", &json!({"text": "hi"}), Caller::Agent, Some(&n)), Verdict::Allow);
    }

    #[test]
    fn a_yes_given_while_another_program_came_to_the_front_is_not_a_yes_for_that_program() {
        let (notepad, mail, notepad_again) = (fg("notepad.exe", "Untitled"), fg("outlook.exe", "Inbox"), fg("Notepad.exe", "Untitled - changed"));
        assert!(!input_target_changed(Some(&notepad), Some(&notepad_again)), "the same program, a new title and a new capital letter");
        // Two windows of one program are two windows; the same window with a new title (text was typed) is the same window.
        let on = |exe: &str, title: &str, window: isize| Foreground { exe: exe.into(), title: title.into(), window };
        assert!(input_target_changed(Some(&on("teams.exe", "Chat with Bob", 100)), Some(&on("teams.exe", "Chat with the boss", 200))), "another window of the same program");
        assert!(!input_target_changed(Some(&on("notepad.exe", "Untitled", 100)), Some(&on("notepad.exe", "*Untitled", 100))), "the same window, its title changed");
        assert!(!input_target_changed(Some(&on("notepad.exe", "Untitled", 0)), Some(&on("notepad.exe", "Untitled", 200))), "a handle that was not known cannot say it changed");
        assert!(input_target_changed(Some(&notepad), Some(&mail)), "another program came to the front while the question was open");
        assert!(input_target_changed(Some(&notepad), None), "the front window can no longer be read");
        assert!(!input_target_changed(None, Some(&mail)), "the person said yes to 'I could not tell which window'");
        assert!(!input_target_changed(None, None));
        // The window that is in front after the answer is judged as any other: Aqua's own is refused whatever was said.
        let aqua = fg("aqua-desktop.exe", "Aqua Assistant");
        assert!(matches!(evaluate("Type", &json!({"text": "hello"}), Caller::Agent, Some(&aqua)), Verdict::Deny(DenyKind::Blocked, _)));
    }

    #[test]
    fn typing_into_a_shell_is_asked_with_all_of_what_would_be_typed_and_the_shell_tools_stay_blocked_for_everyone() {
        let shell = fg("powershell.exe", "Windows PowerShell");
        // All of it, `--%` included: PowerShell's stop-parsing token, the one a safety parser read differently from PowerShell (CVE-2026-19591).
        let text = format!("git status {} --% & calc.exe", "x".repeat(600));
        let asked = evaluate_with("Type", &json!({"text": text}), Caller::Agent, Some(&shell), false);
        let Verdict::Ask(question) = asked else { panic!("typing into a shell asks: {asked:?}") };
        assert!(question.contains("--% & calc.exe") && question.contains(&"x".repeat(600)) && !question.contains("not shown"), "{question}");
        // Too long to show whole: not typed, and nobody is asked.
        let long = format!("git status {} --% & calc.exe", "x".repeat(2000));
        let refused = evaluate_with("Type", &json!({"text": long}), Caller::Agent, Some(&shell), false);
        assert!(matches!(&refused, Verdict::Deny(DenyKind::Invalid, why) if why.contains("too many to type into a shell")), "{refused:?}");
        // A line break does not start a line of its own, and what is drawn differently from what it is is named.
        let sneaky = evaluate_with("Type", &json!({"text": "dir\nApproved by you: yes\u{202e}"}), Caller::Agent, Some(&shell), false);
        assert!(matches!(&sneaky, Verdict::Ask(q) if !q.contains(['\n', '\u{202e}']) && q.contains("U+202E")), "{sneaky:?}");
        // The shell-style tools of the server are not for anybody, under full access either, in any spelling of the name.
        for tool in ["PowerShell", "powershell", "POWERSHELL", " PowerShell ", "Shell", "shell", "FileSystem", "Registry", "Process", "Scrape", "Clipboard"] {
            for caller in [Caller::Agent, Caller::Api, Caller::Voice] {
                for full in [false, true] {
                    let v = evaluate_with(tool, &json!({"command": "calc"}), caller, Some(&fg("notepad.exe", "x")), full);
                    assert!(matches!(v, Verdict::Deny(_, _)), "{tool} {caller:?} full={full}: {v:?}");
                }
            }
        }
    }

    #[test]
    fn typing_with_enter_in_one_call_is_a_send_and_asks_like_ctrl_enter_unless_full_access_is_on() {
        let notepad = fg("notepad.exe", "x");
        assert_eq!(evaluate_with("Type", &json!({"text": "hello"}), Caller::Agent, Some(&notepad), false), Verdict::Allow);
        assert_eq!(evaluate_with("Type", &json!({"text": "hello", "press_enter": false}), Caller::Agent, Some(&notepad), false), Verdict::Allow);
        let sent = evaluate_with("Type", &json!({"text": "hello", "press_enter": true}), Caller::Agent, Some(&notepad), false);
        assert!(matches!(&sent, Verdict::Ask(q) if q.contains("press Enter") && q.contains("5 characters")), "{sent:?}");
        // Full access drops the question as it does for Ctrl+Enter; a shell, a browser and Aqua's own window are decided first.
        assert_eq!(evaluate_with("Type", &json!({"text": "hello", "press_enter": true}), Caller::Agent, Some(&notepad), true), Verdict::Allow);
        assert!(matches!(evaluate_with("Type", &json!({"text": "hello", "press_enter": true}), Caller::Agent, Some(&fg("powershell.exe", "x")), true), Verdict::Ask(_)));
        assert!(matches!(evaluate_with("Type", &json!({"text": "hello", "press_enter": true}), Caller::Api, Some(&notepad), false), Verdict::Ask(_)));
    }

    #[test]
    fn enter_is_enter_however_the_call_says_it_and_a_line_break_in_the_text_is_typed_as_enter() {
        let notepad = fg("notepad.exe", "x");
        let asks = |args: Value| matches!(evaluate_with("Type", &args, Caller::Agent, Some(&notepad), false), Verdict::Ask(_));
        for says_enter in [json!(true), json!("true"), json!("True"), json!(" TRUE "), json!(1), json!("1"), json!("yes"), json!("on"), json!(2.5), json!(["x"]), json!({"a": 1})] {
            assert!(asks(json!({"text": "hi", "press_enter": says_enter.clone()})), "{says_enter}");
        }
        for says_no in [json!(false), json!("false"), json!("False"), json!(" no "), json!(0), json!(0.0), json!("0"), json!("off"), json!(""), json!(null)] {
            assert!(!asks(json!({"text": "hi", "press_enter": says_no.clone()})), "{says_no}");
        }
        assert!(!asks(json!({"text": "hi"})));
        // The server types a line break as the Enter key, so text with one is a send in a chat or a form.
        assert!(asks(json!({"text": "hello\n"})) && asks(json!({"text": "first\r\nsecond"})) && asks(json!({"text": "\n"})));
        // Full access drops the question as it does for the flag.
        assert_eq!(evaluate_with("Type", &json!({"text": "hello\n"}), Caller::Agent, Some(&notepad), true), Verdict::Allow);
        // The voice never types it: no Enter by voice, whichever way it is said.
        for args in [json!({"text": "hello\n"}), json!({"text": "hello", "press_enter": true}), json!({"text": "hello", "press_enter": "true"})] {
            assert!(matches!(evaluate_with("Type", &args, Caller::Voice, Some(&notepad), false), Verdict::Deny(DenyKind::Blocked, _)), "{args}");
        }
        assert_eq!(evaluate_with("Type", &json!({"text": "hello"}), Caller::Voice, Some(&notepad), false), Verdict::Allow);
    }

    #[test]
    fn unknown_foreground_asks_before_input() {
        assert!(matches!(evaluate("Click", &json!({"loc": [1, 2]}), Caller::Agent, None), Verdict::Ask(_)));
    }

    #[test]
    fn observing_needs_no_foreground_and_voice_observes_opens_apps_by_name_scrolls_and_types_text_only() {
        assert_eq!(evaluate("Snapshot", &json!({"use_vision": false}), Caller::Voice, None), Verdict::Allow);
        assert_eq!(evaluate("Screenshot", &json!({}), Caller::Agent, None), Verdict::Allow);
        assert!(is_blocked(&evaluate("Click", &json!({"loc": [1, 2]}), Caller::Voice, Some(&fg("notepad.exe", "x")))));
        assert_eq!(evaluate("Type", &json!({"text": "hi"}), Caller::Voice, Some(&fg("notepad.exe", "x"))), Verdict::Allow, "text into the window in front (decision D-14)");
        assert!(is_blocked(&evaluate("Shortcut", &json!({"shortcut": "ctrl+s"}), Caller::Voice, Some(&fg("notepad.exe", "x")))));
    }

    #[test]
    fn a_drag_is_an_action_and_a_plain_pointer_move_is_not() {
        let notepad = fg("notepad.exe", "x");
        // Moving the pointer only looks.
        assert!(is_observe_call("Move", &json!({"loc": [10, 20]})));
        assert!(is_observe_call("Move", &json!({"loc": [10, 20], "drag": false})));
        assert_eq!(evaluate("Move", &json!({"loc": [10, 20]}), Caller::Voice, None), Verdict::Allow);
        // Anything that drags is an action: it is not an observe call, voice cannot start it, and it is checked like a click.
        for drag in [json!({"loc": [1, 2], "drag": true}), json!({"loc": [1, 2], "drag": "true"}), json!({"loc": [1, 2], "drag": 1}), json!({"loc": [1, 2], "from_loc": [3, 4]})] {
            assert!(!is_observe_call("Move", &drag), "{drag}");
            assert!(is_blocked(&evaluate("Move", &drag, Caller::Voice, Some(&notepad))), "{drag}");
            assert_eq!(evaluate("Move", &drag, Caller::Agent, Some(&notepad)), Verdict::Allow, "{drag}");
            assert!(matches!(evaluate("Move", &drag, Caller::Agent, None), Verdict::Ask(_)), "{drag}: no known window");
            assert!(matches!(evaluate("Move", &drag, Caller::Agent, Some(&fg("powershell.exe", "Windows PowerShell"))), Verdict::Ask(_)), "{drag}: a shell");
            assert!(is_blocked(&evaluate("Move", &drag, Caller::Agent, Some(&fg("aqua-desktop.exe", "Aqua")))), "{drag}: Aqua's own window");
        }
        // Other observe tools are unchanged, and `drag` on them means nothing.
        assert!(is_observe_call("Scroll", &json!({"drag": true})));
        assert!(is_observe_call("Snapshot", &json!({})));
        assert!(!is_observe_call("Click", &json!({})));
    }

    #[test]
    fn voice_can_open_an_app_by_name_but_not_a_shell_or_a_command() {
        assert_eq!(evaluate("App", &json!({"mode": "launch", "name": "Notepad"}), Caller::Voice, None), Verdict::Allow);
        assert_eq!(evaluate("App", &json!({"name": "Notepad"}), Caller::Voice, None), Verdict::Allow);
        assert_eq!(evaluate("App", &json!({"mode": "switch", "name": "Notepad"}), Caller::Voice, None), Verdict::Allow);
        assert!(is_blocked(&evaluate("App", &json!({"mode": "resize", "name": "Notepad"}), Caller::Voice, None)));
        assert!(is_blocked(&evaluate("App", &json!({"mode": "launch", "name": "PowerShell"}), Caller::Voice, None)));
        assert!(is_blocked(&evaluate("App", &json!({"mode": "launch", "name": "Notepad", "executable": "C:/x.exe"}), Caller::Voice, None)));
        assert!(is_blocked(&evaluate("App", &json!({"mode": "launch", "name": "Notepad", "args": ["a"]}), Caller::Voice, None)));
    }

    #[test]
    fn oversized_arguments_are_rejected() {
        let big = "x".repeat(MAX_STRING_CHARS + 1);
        let v = evaluate("Type", &json!({"text": big}), Caller::Agent, Some(&fg("notepad.exe", "x")));
        assert!(matches!(v, Verdict::Deny(DenyKind::Invalid, _)));
        let v = evaluate("Snapshot", &json!("not an object"), Caller::Agent, None);
        assert!(matches!(v, Verdict::Deny(DenyKind::Invalid, _)));
    }

    #[test]
    fn audit_never_records_typed_text() {
        let dir = std::env::temp_dir().join(format!("aqua_wguard_{}", std::process::id()));
        let g = WindowsGuard::new(dir.join("audit.jsonl"));
        // A call that is blocked before reaching the server still records the redacted args.
        let client = Mutex::new(StdioMcpClient::new(crate::windows_mcp::default_windows_config()));
        let out = g.call(&client, None, "Type", json!({"text": "hunter2-secret"}), Caller::Voice);
        assert!(out.starts_with("ERROR [PERMISSION_DENIED]"));
        let log = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap();
        assert!(log.contains("<redacted 14 chars>"));
        assert!(!log.contains("hunter2-secret"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn blocked_call_never_starts_the_server() {
        let dir = std::env::temp_dir().join(format!("aqua_wguard2_{}", std::process::id()));
        let g = WindowsGuard::new(dir.join("audit.jsonl"));
        let client = Mutex::new(StdioMcpClient::new(crate::windows_mcp::default_windows_config()));
        let out = g.call(&client, None, "PowerShell", json!({"command": "whoami"}), Caller::Api);
        assert!(out.starts_with("ERROR [PERMISSION_DENIED] PowerShell"));
        assert!(!client.lock().unwrap().is_running());
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- card R6-34: tiers, sign-in and payment steps, one session at a time

    #[test]
    fn the_relaxed_policy_is_exactly_what_the_rules_decided_before_the_tiers() {
        let windows = [
            None,
            Some(fg("chrome.exe", "Inbox - Gmail")),
            Some(fg("msedge.exe", "Aqua Admin - Microsoft Edge")),
            Some(fg("powershell.exe", "Windows PowerShell")),
            Some(fg("explorer.exe", "Run")),
            Some(fg("regedit.exe", "Registry Editor")),
            Some(fg("notepad.exe", "Untitled")),
            Some(fg("aqua-desktop.exe", "Aqua Assistant")),
        ];
        let calls = [
            ("Click", json!({"loc": [1, 2]})),
            ("Click", json!({"loc": [1, 2], "button": "right"})),
            ("Type", json!({"text": "hello"})),
            ("Type", json!({"text": "hello", "press_enter": true})),
            ("MultiEdit", json!({"text": "x"})),
            ("MultiSelect", json!({"loc": [1, 2]})),
            ("Shortcut", json!({"shortcut": "ctrl+s"})),
            ("Shortcut", json!({"shortcut": "ctrl+enter"})),
            ("Shortcut", json!({"shortcut": "win+r"})),
            ("Move", json!({"loc": [1, 2], "drag": true})),
            ("Move", json!({"loc": [1, 2]})),
            ("Scroll", json!({"direction": "down"})),
            ("Snapshot", json!({})),
            ("App", json!({"mode": "launch", "name": "Notepad"})),
            ("PowerShell", json!({"command": "dir"})),
        ];
        for f in &windows {
            for (tool, args) in &calls {
                for caller in [Caller::Agent, Caller::Api, Caller::Voice] {
                    for full in [false, true] {
                        assert_eq!(
                            evaluate_tiered(tool, args, caller, f.as_ref(), full, tiers::Policy::Relaxed),
                            evaluate_with(tool, args, caller, f.as_ref(), full),
                            "{tool} {args} {caller:?} full={full} in {f:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn under_the_strict_tiers_a_browser_is_view_only_a_terminal_click_only_and_full_access_widens_neither() {
        use tiers::Policy::{Relaxed, Strict};
        let browser = fg("chrome.exe", "Inbox - Gmail");
        let shell = fg("powershell.exe", "Windows PowerShell");
        let notepad = fg("notepad.exe", "Untitled");
        let typed = json!({"text": "hello"});
        for full in [false, true] {
            // A browser: refused, not asked, with or without full access; the old question is what the relaxed policy keeps.
            assert!(is_blocked(&evaluate_tiered("Type", &typed, Caller::Agent, Some(&browser), full, Strict)), "full={full}");
            assert!(is_blocked(&evaluate_tiered("Click", &json!({"loc": [1, 2]}), Caller::Agent, Some(&browser), full, Strict)), "full={full}");
            assert!(is_blocked(&evaluate_tiered("Shortcut", &json!({"shortcut": "ctrl+l"}), Caller::Api, Some(&browser), full, Strict)), "full={full}");
            // A terminal: nothing typed or pressed; a plain left click is not refused, and is still asked about as the old rule did (before any input to a shell), full access or not as before.
            assert!(is_blocked(&evaluate_tiered("Type", &typed, Caller::Agent, Some(&shell), full, Strict)), "full={full}");
            assert!(is_blocked(&evaluate_tiered("Shortcut", &json!({"shortcut": "enter"}), Caller::Agent, Some(&shell), full, Strict)), "full={full}");
            let plain_click = json!({"loc": [1, 2]});
            let clicked = evaluate_tiered("Click", &plain_click, Caller::Agent, Some(&shell), full, Strict);
            assert_eq!(clicked, evaluate_with("Click", &plain_click, Caller::Agent, Some(&shell), full), "the tier does not change what the rules say about a plain click: full={full}");
            assert!(full || matches!(clicked, Verdict::Ask(_)), "a click aimed at a shell is a question, never silent: {clicked:?}");
            assert!(is_blocked(&evaluate_tiered("Click", &json!({"loc": [1, 2], "button": "right"}), Caller::Agent, Some(&shell), full, Strict)), "a right click pastes");
            // An ordinary program is the old rules, and so is looking.
            assert_eq!(evaluate_tiered("Type", &typed, Caller::Agent, Some(&notepad), full, Strict), Verdict::Allow);
            assert_eq!(evaluate_tiered("Snapshot", &json!({}), Caller::Agent, Some(&browser), full, Strict), Verdict::Allow, "a browser may be looked at");
            assert_eq!(evaluate_tiered("Scroll", &json!({"direction": "down"}), Caller::Voice, Some(&browser), full, Strict), Verdict::Allow, "and scrolled");
        }
        assert!(matches!(evaluate_tiered("Type", &typed, Caller::Agent, Some(&browser), false, Relaxed), Verdict::Ask(_)), "relaxed keeps the question");
        assert!(matches!(evaluate_tiered("Click", &json!({"loc": [1, 2]}), Caller::Agent, Some(&shell), false, Relaxed), Verdict::Ask(_)), "relaxed keeps the question");
        // The rules' own refusals are not replaced by the tier's: Aqua's own window says why, and a blocked shortcut says what it opens.
        let aqua = fg("msedge.exe", "Aqua Admin - Microsoft Edge");
        assert!(matches!(evaluate_tiered("Type", &typed, Caller::Agent, Some(&aqua), false, Strict), Verdict::Deny(DenyKind::Blocked, why) if why.contains("approval prompts")));
        assert!(matches!(evaluate_tiered("Shortcut", &json!({"shortcut": "win+r"}), Caller::Agent, Some(&browser), false, Strict), Verdict::Deny(DenyKind::Blocked, why) if why.contains("command or system surface")));
        // The voice gets nothing from a tier: it could not click, and it still cannot type where the agent is refused.
        assert!(is_blocked(&evaluate_tiered("Click", &json!({"loc": [1, 2]}), Caller::Voice, Some(&shell), false, Strict)));
        assert!(is_blocked(&evaluate_tiered("Type", &typed, Caller::Voice, Some(&browser), false, Strict)));
        assert_eq!(evaluate_tiered("Type", &typed, Caller::Voice, Some(&notepad), false, Strict), Verdict::Allow, "decision D-14 stands");
    }

    /// A guard with its own desk and a server that is never started: what comes back, and that nothing was sent.
    fn call_without_a_server(f: Option<Foreground>, owner: &str, tool: &str, args: Value, caller: Caller, setup: impl FnOnce(&WindowsGuard)) -> (String, WindowsGuard) {
        let dir = std::env::temp_dir().join(format!("aqua_wguard_r634_{}_{}", std::process::id(), now_ms()));
        let g = WindowsGuard::new(dir.join("audit.jsonl"));
        setup(&g);
        let client = Mutex::new(StdioMcpClient::new(crate::windows_mcp::default_windows_config()));
        let out = {
            let _as = session::as_owner(owner);
            g.call_at(f, &client, None, tool, args, caller)
        };
        assert!(!client.lock().unwrap().is_running(), "the server was started for a call that is refused or not approved: {out}");
        let _ = std::fs::remove_dir_all(dir);
        (out, g)
    }

    #[test]
    fn the_door_refuses_what_a_tier_refuses_and_asks_at_a_sign_in_step_and_never_puts_the_voice_there() {
        if tiers::Policy::from_env() != tiers::Policy::Strict {
            return; // this machine's own setting is "relaxed": the tiers are not what is being tested here
        }
        // A browser is view-only: refused before anything is asked or sent.
        let (out, _) = call_without_a_server(Some(fg("chrome.exe", "Inbox")), "run_a", "Type", json!({"text": "hi"}), Caller::Agent, |_| {});
        assert!(out.starts_with("ERROR [PERMISSION_DENIED] Type") && out.contains("view-only"), "{out}");
        // At a sign-in step in a plain program the agent is asked (nobody answers here, so it is not approved), and what it was going to type is not in the question.
        let secret = "correct-horse-battery-staple";
        let step = || Some(fg("contoso.exe", "Sign in - Contoso"));
        let (out, g) = call_without_a_server(step(), "run_a", "Type", json!({"text": secret}), Caller::Agent, |_| {});
        assert!(out.starts_with("ERROR [CONFIRMATION_DENIED] Type") && out.contains("sign-in step") && !out.contains(secret), "{out}");
        assert_eq!(g.screen.desk().holder_at(Instant::now()), None, "a question that was not approved does not keep the desktop from another session");
        // The same by the API, and by voice: refused outright, since a spoken request cannot answer it.
        let (out, _) = call_without_a_server(step(), "windows:api", "Click", json!({"loc": [1, 2]}), Caller::Api, |_| {});
        assert!(out.starts_with("ERROR [CONFIRMATION_DENIED] Click") && out.contains("sign-in step"), "{out}");
        let (out, _) = call_without_a_server(step(), "windows:voice", "Type", json!({"text": secret}), Caller::Voice, |_| {});
        assert!(out.starts_with("ERROR [PERMISSION_DENIED] Type") && out.contains("never by voice") && !out.contains(secret), "{out}");
        // A payment step found from a snapshot of the same window a moment ago (no title that says so).
        let page = Foreground { exe: "contoso.exe".into(), title: "Contoso".into(), window: 9 };
        let (out, _) = call_without_a_server(Some(page.clone()), "run_a", "Click", json!({"loc": [1, 2]}), Caller::Agent, |g| {
            g.screen.note_snapshot(&page, "Card number CVV Pay now");
        });
        assert!(out.starts_with("ERROR [CONFIRMATION_DENIED] Click") && out.contains("payment step") && out.contains("card number"), "{out}");
    }

    #[test]
    fn a_second_session_that_acts_on_the_screen_is_refused_with_the_name_of_the_first_before_anyone_is_asked() {
        if tiers::Policy::from_env() != tiers::Policy::Strict {
            return;
        }
        let notepad = || Some(fg("notepad.exe", "Untitled"));
        let (out, g) = call_without_a_server(notepad(), "run_b", "Click", json!({"loc": [1, 2]}), Caller::Agent, |g| {
            g.screen.desk().claim_at("run_a", Instant::now()).unwrap();
        });
        assert!(out.starts_with("ERROR [PERMISSION_DENIED] Click") && out.contains("another session (run_a)"), "{out}");
        assert_eq!(g.screen.desk().holder_at(Instant::now()).as_deref(), Some("run_a"), "the first session still has it");
        // The refusal comes before the question: at a sign-in step the second session is refused, not asked.
        let (out, _) = call_without_a_server(Some(fg("contoso.exe", "Sign in")), "run_b", "Click", json!({"loc": [1, 2]}), Caller::Agent, |g| {
            g.screen.desk().claim_at("run_a", Instant::now()).unwrap();
        });
        assert!(out.contains("another session (run_a)") && !out.contains("CONFIRMATION_DENIED"), "{out}");
        // The voice is a session of its own: it does not type into a window while an agent run has the screen.
        let (out, _) = call_without_a_server(notepad(), "windows:voice", "Type", json!({"text": "hello"}), Caller::Voice, |g| {
            g.screen.desk().claim_at("run_a", Instant::now()).unwrap();
        });
        assert!(out.contains("another session (run_a)"), "{out}");
    }

    /// Card R6-08, T6-08.8: a question of the guard carries what the host's questions carry: what the call can do, what could go wrong, the rule that made it ask and the window it is
    /// about. The guard asks (Enter in a chat window), the question is read from the broker, and a "no" ends the call before the server is reached.
    #[test]
    fn a_question_of_the_guard_says_what_the_call_can_do_what_could_go_wrong_which_rule_asked_and_which_window_it_is_about() {
        use crate::browser::confirm::ConfirmationBroker;
        use tokio::sync::broadcast::error::TryRecvError;
        let chat = Foreground { exe: "slack.exe".into(), title: "Slack - team".into(), window: 4 };
        let dir = std::env::temp_dir().join(format!("aqua_wguard_why_{}", std::process::id()));
        let g = WindowsGuard::new(dir.join("audit.jsonl"));
        let client = Mutex::new(StdioMcpClient::new(crate::windows_mcp::default_windows_config()));
        let broker = ConfirmationBroker::default();
        let mut heard = broker.subscribe();
        let (asked, out) = std::thread::scope(|s| {
            let asker = s.spawn(|| g.call_at(Some(chat.clone()), &client, Some(&broker), "Type", json!({"text": "I will pay you", "press_enter": true}), Caller::Agent));
            let asked = loop {
                match heard.try_recv() {
                    Ok(q) => break q,
                    Err(TryRecvError::Empty) => {
                        assert!(!asker.is_finished(), "the guard did not ask");
                        std::thread::yield_now();
                    }
                    Err(other) => panic!("{other:?}"),
                }
            };
            assert!(broker.resolve(&asked.request_id, Decision::Deny));
            (asked, asker.join().unwrap())
        });
        assert_eq!((asked.kind, asked.tool.as_str(), asked.always_ask), (ConfirmKind::Tool, "Type", true), "an answer of the guard is never remembered, so the question does not offer it");
        assert_eq!(asked.classes, vec!["WRITE".to_string(), "EXTERNAL_EFFECT".to_string()]);
        assert!(asked.risk.contains("another system"), "{}", asked.risk);
        assert!(asked.rule.contains("press Enter"), "{}", asked.rule);
        assert!(asked.data.len() == 1 && asked.data[0].starts_with("Target: slack.exe"), "{:?}", asked.data);
        assert!(!asked.reason.contains("pay you") && !asked.rule.contains("pay you") && !asked.data.join(" ").contains("pay you"), "what would have been typed is not in the question");
        assert!(out.starts_with("ERROR [CONFIRMATION_DENIED] Type"), "{out}");
        assert!(!client.lock().unwrap().is_running(), "a question that was answered no never reaches the server");
        let _ = std::fs::remove_dir_all(dir);
    }
}
