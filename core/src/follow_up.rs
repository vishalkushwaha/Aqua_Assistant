//! A short session after Aqua does one step, so the next sentence can continue on the same target.
//!
//! The target is whatever was just opened or focused (an app, a page, a file). The next utterance can ask to read it,
//! type into it, dictate into it, or leave it. A new task or an ordinary question ends the session. Dictation types each
//! finished phrase with the existing Windows Type tool and stops on a spoken stop. Whisper is not involved here: the
//! pipeline has already transcribed the phrase.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use aqua_mcp_adapters::windows_mcp::guard::{WindowsGuard, Caller as WindowsCaller};
use serde_json::json;

const IDLE: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    App,
    File,
    Page,
    Search,
}

enum Mode {
    /// Waiting for the user to say what to do with the target.
    Offer,
    /// The next phrases are typed into the target until stop.
    Dictate,
}

struct Session {
    target: String,
    kind: Kind,
    mode: Mode,
    at: Instant,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<Session>> {
    SESSION.lock().unwrap_or_else(|p| p.into_inner())
}

/// Remember a target and return the reply plus one question that fits that kind of target.
pub fn after_step(reply: &str, target: &str, kind: Kind) -> String {
    let target = target.trim();
    if target.is_empty() || reply.contains('?') {
        return reply.to_string();
    }
    *lock() = Some(Session { target: target.to_string(), kind, mode: Mode::Offer, at: Instant::now() });
    match kind {
        Kind::App => crate::conversation::note_app(target),
        Kind::File => crate::conversation::note_file(target),
        Kind::Page | Kind::Search => {}
    }
    let question = match kind {
        Kind::App => format!("Do you want me to read {target}, write in it, or leave it?"),
        Kind::File => format!("Do you want me to read {target}, search in it, or leave it?"),
        Kind::Page => format!("Do you want me to read {target}, scroll it, click something, or leave it?"),
        Kind::Search => format!("Do you want me to open one of those, search again, or leave it?"),
    };
    if reply.trim().is_empty() { question } else { format!("{reply} {question}") }
}

/// What kind of thing this request was about, so the follow-up question matches it.
pub fn kind_of(prompt: &str) -> Kind {
    let p = norm(prompt);
    if p.contains("folder") || p.contains("file") || p.contains(".pdf") || p.contains("document") {
        Kind::File
    } else if p.contains("page") || p.contains("site") || p.contains("browser") || p.contains("http") || p.contains("gmail") || p.contains("mail") || p.contains("youtube") {
        Kind::Page
    } else {
        Kind::App
    }
}

/// "open Notepad and write hello" → ("open Notepad", "hello"). None when it is only an open.
pub fn split_open_and_write(prompt: &str) -> Option<(String, String)> {
    let raw = prompt.trim();
    let lower = norm(raw);
    let (lead, rest) = [" and write ", " and type ", " then write ", " then type "].iter().find_map(|sep| {
        let i = lower.find(sep)?;
        Some((raw.chars().take(i).collect::<String>(), raw.chars().skip(i + sep.len()).collect::<String>()))
    })?;
    let lead = lead.trim().to_string();
    let rest = rest.trim().trim_end_matches(['.', '!', '?']).to_string();
    if lead.is_empty() || rest.is_empty() || !norm(&lead).starts_with("open ") {
        return None;
    }
    Some((lead, rest))
}

pub fn clear() {
    *lock() = None;
}

/// Continue the session, or `None` when this utterance is a new request and the session should not answer it.
pub fn continue_turn(
    prompt: &str,
    windows: &Option<std::sync::Arc<std::sync::Mutex<aqua_mcp_adapters::StdioMcpClient>>>,
    broker: Option<&aqua_mcp_adapters::browser::ConfirmationBroker>,
    spoken: bool,
) -> Option<String> {
    let caller = caller_for(spoken);
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return None;
    }
    let mut guard = lock();
    let session = guard.as_mut()?;
    if session.at.elapsed() > IDLE {
        *guard = None;
        return None;
    }
    if crate::voice_commands::is_stop_command(prompt, "Hey Aqua") {
        *guard = None;
        return Some("Stopped. I left it open.".into());
    }
    if is_new_task(prompt) {
        *guard = None;
        return None;
    }
    let target = session.target.clone();
    match session.mode {
        Mode::Dictate => {
            session.at = Instant::now();
            drop(guard);
            return Some(type_into(&target, prompt, windows, broker, caller));
        }
        Mode::Offer => {}
    }
    let kind = session.kind;
    if is_done(prompt) {
        *guard = None;
        return Some(format!("All right. I'll leave {target}."));
    }
    if is_read(prompt) {
        session.at = Instant::now();
        drop(guard);
        return Some(read_target(&target, windows));
    }
    if is_dictate(prompt) {
        session.mode = Mode::Dictate;
        session.at = Instant::now();
        drop(guard);
        return Some(format!("I'm listening. I'll type what you say into {target}. Say Stop Aqua when you want me to stop."));
    }
    if let Some(text) = text_to_type(prompt) {
        session.at = Instant::now();
        drop(guard);
        return Some(type_into(&target, &text, windows, broker, caller));
    }
    if let Some(dir) = scroll_direction(prompt) {
        session.at = Instant::now();
        drop(guard);
        return Some(scroll_target(&target, dir, windows, broker, caller));
    }
    if let Some(label) = click_label(prompt) {
        session.at = Instant::now();
        drop(guard);
        return Some(click_target(&target, &label, windows, broker, caller));
    }
    if is_send(prompt) {
        session.at = Instant::now();
        drop(guard);
        return Some(send_target(&target, windows, broker, caller));
    }
    if let Some(query) = search_query(prompt) {
        session.at = Instant::now();
        session.kind = Kind::Search;
        session.target = query.clone();
        drop(guard);
        return Some(search_near(&query, kind));
    }
    // A question or a change of subject is not part of this session.
    *guard = None;
    None
}

fn is_done(prompt: &str) -> bool {
    let p = norm(prompt);
    matches!(p.as_str(), "no" | "nope" | "nothing" | "nothing else" | "leave it" | "thats all" | "that is all" | "done" | "im done" | "i am done" | "no thanks")
}

fn is_read(prompt: &str) -> bool {
    let p = norm(prompt);
    p == "read" || p == "read it" || p == "read this" || p.starts_with("read ") || p.contains("what is written") || p.contains("whats written") || p.contains("what does it say")
}

fn is_dictate(prompt: &str) -> bool {
    let p = norm(prompt);
    p.contains("what im saying")
        || p.contains("what i am saying")
        || p.contains("what i say")
        || p.contains("take notes")
        || p.contains("take a note")
        || p.contains("dictat")
        || p == "write" && p.split_whitespace().count() < 4
        || p.starts_with("write what")
        || p.starts_with("type what")
}

fn text_to_type(prompt: &str) -> Option<String> {
    let raw = prompt.trim();
    let lower = norm(raw);
    for lead in ["write ", "type ", "put "] {
        if let Some(rest) = lower.strip_prefix(lead) {
            if rest.split_whitespace().count() >= 1 && !rest.starts_with("what") {
                let n = lead.len();
                return Some(raw.chars().skip(n).collect::<String>().trim().to_string()).filter(|s| !s.is_empty());
            }
        }
    }
    None
}

fn is_new_task(prompt: &str) -> bool {
    let p = norm(prompt);
    if p.contains("weather") || p.contains("temperature") {
        return true;
    }
    ["open ", "launch ", "start ", "go to ", "search my ", "find my ", "find the "].iter().any(|s| p.starts_with(s))
}

fn scroll_direction(prompt: &str) -> Option<&'static str> {
    let p = norm(prompt);
    if p.contains("scroll up") || p == "up" { Some("up") } else if p.contains("scroll") || p == "down" { Some("down") } else { None }
}

fn click_label(prompt: &str) -> Option<String> {
    let raw = prompt.trim();
    let lower = norm(raw);
    let rest = lower.strip_prefix("click ")?.trim();
    if rest.is_empty() { return None; }
    Some(raw.chars().skip("click ".len()).collect::<String>().trim().trim_end_matches(['.', '!', '?']).to_string())
}

fn is_send(prompt: &str) -> bool {
    let p = norm(prompt);
    matches!(p.as_str(), "send" | "send it" | "send this" | "send the message")
}

fn search_query(prompt: &str) -> Option<String> {
    let raw = prompt.trim();
    let lower = norm(raw);
    for lead in ["search for ", "search in it for ", "search again for ", "look for "] {
        if let Some(rest) = lower.strip_prefix(lead) {
            if rest.is_empty() { return None; }
            return Some(raw.chars().skip(lead.len()).collect::<String>().trim().to_string());
        }
    }
    None
}

fn norm(prompt: &str) -> String {
    prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', '!', '?'])
        .to_ascii_lowercase()
        .replace('\'', "")
}

/// Who is acting for a follow-up: what was typed in the app is the person's own deliberate request (the agent's powers); what was said aloud is a spoken request (it may
/// type text into the window in front and scroll, and nothing that clicks, presses Enter or sends: the guard decides, see `WindowsGuard`). A follow-up used to act as the agent
/// whoever had asked, so "click Send" and "send it" said aloud ran.
pub fn caller_for(spoken: bool) -> WindowsCaller {
    if spoken {
        WindowsCaller::Voice
    } else {
        WindowsCaller::Agent
    }
}

#[allow(clippy::too_many_arguments)]
fn act(target: &str, tool: &str, args: serde_json::Value, windows: &Option<std::sync::Arc<std::sync::Mutex<aqua_mcp_adapters::StdioMcpClient>>>, broker: Option<&aqua_mcp_adapters::browser::ConfirmationBroker>, ok: &str, caller: WindowsCaller) -> String {
    let Some(client) = windows else {
        return format!("I can't do that in {target} from here.");
    };
    // The guard decides by who asks and still refuses shells and Aqua's own window, and asks before a send into a browser (never of a spoken request).
    let obs = WindowsGuard::global().call(client, broker, tool, args, caller);
    let lower = obs.to_ascii_lowercase();
    if lower.contains("permission_denied") || lower.contains("confirmation_denied") || lower.contains("error") {
        format!("I didn't do that in {target}. {}", obs.chars().take(180).collect::<String>())
    } else {
        ok.to_string()
    }
}

fn type_into(target: &str, text: &str, windows: &Option<std::sync::Arc<std::sync::Mutex<aqua_mcp_adapters::StdioMcpClient>>>, broker: Option<&aqua_mcp_adapters::browser::ConfirmationBroker>, caller: WindowsCaller) -> String {
    act(target, "Type", json!({ "text": text }), windows, broker, &format!("Typed that into {target}."), caller)
}

fn scroll_target(target: &str, direction: &str, windows: &Option<std::sync::Arc<std::sync::Mutex<aqua_mcp_adapters::StdioMcpClient>>>, broker: Option<&aqua_mcp_adapters::browser::ConfirmationBroker>, caller: WindowsCaller) -> String {
    act(target, "Scroll", json!({ "direction": direction, "wheel_times": 3, "type": "vertical" }), windows, broker, &format!("Scrolled {direction} in {target}."), caller)
}

fn click_target(target: &str, label: &str, windows: &Option<std::sync::Arc<std::sync::Mutex<aqua_mcp_adapters::StdioMcpClient>>>, broker: Option<&aqua_mcp_adapters::browser::ConfirmationBroker>, caller: WindowsCaller) -> String {
    // The Windows tool clicks an element by the label number from the last snapshot, or by these words.
    act(target, "Click", json!({ "loc": label }), windows, broker, &format!("Clicked {label} in {target}."), caller)
}

fn send_target(target: &str, windows: &Option<std::sync::Arc<std::sync::Mutex<aqua_mcp_adapters::StdioMcpClient>>>, broker: Option<&aqua_mcp_adapters::browser::ConfirmationBroker>, caller: WindowsCaller) -> String {
    act(target, "Shortcut", json!({ "shortcut": "ctrl+enter" }), windows, broker, &format!("Sent it from {target}."), caller)
}

/// What a spoken sentence asks to have typed into the window in front (decision D-14: a spoken request may type text there, wherever the cursor is, and never press
/// Enter, send or click).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Typing {
    /// "type hello there": the words to type.
    Text(String),
    /// "type hello and press enter": the typing part could be done and the Enter part is not the voice's.
    NeedsEnter,
    /// Not a request to type.
    NotTyping,
}

/// Reads "type ..." (after "please", "Aqua," or "can you") as a request to type the words that follow. "type what I'm saying" is a request to dictate and needs a session;
/// "typewriter prices" and "write me a poem" are not requests to type.
pub fn typing_asked(sentence: &str) -> Typing {
    let raw = sentence.trim();
    let lower = norm(raw);
    let mut start = 0;
    let mut rest = lower.as_str();
    // The ways a request is led in ("Hey Aqua, could you please ..."), in any order and any number.
    loop {
        let before = rest.len();
        for lead in ["hey aqua, ", "hey aqua ", "ok aqua, ", "ok aqua ", "okay aqua, ", "okay aqua ", "aqua, ", "aqua ", "please ", "can you ", "could you ", "would you ", "will you "] {
            if let Some(r) = rest.strip_prefix(lead) {
                start += lead.len();
                rest = r;
            }
        }
        if rest.len() == before {
            break;
        }
    }
    let Some(words) = rest.strip_prefix("type ") else { return Typing::NotTyping };
    if words.trim().is_empty() || words.starts_with("what ") || words.starts_with("out ") || words.starts_with("up ") {
        return Typing::NotTyping;
    }
    // `norm` lower-cases and drops apostrophes, so the text comes from the sentence itself, from the same place.
    let text: String = raw.chars().skip(start + "type ".len()).collect::<String>().trim().trim_end_matches(['.', '!', '?']).trim().to_string();
    let end_words = ["and press enter", "and hit enter", "then press enter", "then hit enter", "and press return", "and send it", "and submit it", "and send", "and submit", "then send it", "then enter"];
    if end_words.iter().any(|e| words.trim_end_matches(['.', '!', '?']).ends_with(e)) {
        return Typing::NeedsEnter;
    }
    if text.is_empty() {
        Typing::NotTyping
    } else {
        Typing::Text(text)
    }
}

/// Types `text` into the window in front, as a spoken request does.
pub fn type_into_the_window_in_front(text: &str, windows: &Option<std::sync::Arc<std::sync::Mutex<aqua_mcp_adapters::StdioMcpClient>>>, broker: Option<&aqua_mcp_adapters::browser::ConfirmationBroker>) -> String {
    type_into("the window in front", text, windows, broker, WindowsCaller::Voice)
}

fn search_near(query: &str, kind: Kind) -> String {
    if kind == Kind::File || kind == Kind::Search {
        if let Some(ix) = aqua_mcp_adapters::file_index::global() {
            let found = ix.lookup(&aqua_mcp_adapters::file_index::LookupRequest { query: query.to_string(), want: aqua_mcp_adapters::file_index::Want::Search, limit: 5 });
            if !found.found.is_empty() {
                let lines: Vec<String> = found.found.iter().take(5).map(|h| h.path.display().to_string()).collect();
                return format!("I found {} for {query}: {}", found.found.len(), lines.join("; "));
            }
        }
    }
    format!("I looked for {query}. Say open and the name if you want one of them, or tell me what to do next.")
}

fn read_target(target: &str, windows: &Option<std::sync::Arc<std::sync::Mutex<aqua_mcp_adapters::StdioMcpClient>>>) -> String {
    let Some(client) = windows else {
        return format!("I can't see {target} from here.");
    };
    let obs = WindowsGuard::global().call(client, None, "Snapshot", json!({ "use_vision": false }), WindowsCaller::Voice);
    let clipped: String = obs.chars().take(600).collect();
    if clipped.trim().is_empty() {
        format!("I couldn't read {target}.")
    } else {
        format!("Here is what I can see for {target}: {clipped}")
    }
}

/// The session is one for the process: a test that starts or ends one takes this first (the tests here, and any test of the pipeline that does).
#[cfg(test)]
pub(crate) static SESSION_TESTS: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn alone<T>(f: impl FnOnce() -> T) -> T {
        let _hold = SESSION_TESTS.lock().unwrap_or_else(|p| p.into_inner());
        f()
    }

    #[test]
    fn a_follow_up_offers_the_target_and_a_new_open_leaves_the_session() {
        alone(|| {
        clear();
        let said = after_step("Opened Mail.", "Mail", Kind::App);
        assert!(said.contains("read Mail"));
        assert!(continue_turn("open Notepad", &None, None, true).is_none());
        assert!(continue_turn("read it", &None, None, true).is_none(), "the session ended when the subject changed");
        })
    }

    #[test]
    fn a_follow_up_acts_with_the_powers_of_who_asked_and_a_spoken_one_is_a_spoken_request() {
        assert_eq!(caller_for(true), WindowsCaller::Voice);
        assert_eq!(caller_for(false), WindowsCaller::Agent);
        // What a spoken request may do, as the guard says it for the four things a follow-up does.
        use aqua_mcp_adapters::windows_mcp::guard::{evaluate_with, Foreground, Verdict};
        let notepad = Foreground { exe: "notepad.exe".into(), title: "Untitled - Notepad".into(), ..Default::default() };
        let by = |tool: &str, args: serde_json::Value| evaluate_with(tool, &args, caller_for(true), Some(&notepad), false);
        assert_eq!(by("Type", json!({"text": "hello"})), Verdict::Allow);
        assert_eq!(by("Scroll", json!({"direction": "down"})), Verdict::Allow);
        assert!(matches!(by("Click", json!({"loc": "Send"})), Verdict::Deny(..)), "click Send said aloud does not click");
        assert!(matches!(by("Shortcut", json!({"shortcut": "ctrl+enter"})), Verdict::Deny(..)), "send it said aloud does not send");
        // Typed in the app, the same follow-up is the person's own request.
        assert_eq!(evaluate_with("Click", &json!({"loc": "Send"}), caller_for(false), Some(&notepad), false), Verdict::Allow);
    }

    #[test]
    fn what_is_asked_to_be_typed_into_the_window_in_front_is_read_from_thirty_sentences() {
        use Typing::*;
        let text = |t: &str| Text(t.to_string());
        for (said, read) in [
            ("type hello", text("hello")),
            ("Type hello there.", text("hello there")),
            ("please type hello world", text("hello world")),
            ("Hey Aqua, type meet me at five", text("meet me at five")),
            ("Aqua, please type I'll be late", text("I'll be late")),
            ("can you type thanks for the update", text("thanks for the update")),
            ("could you please type Dear Anna,", text("Dear Anna,")),
            ("okay Aqua type see you tomorrow!", text("see you tomorrow")),
            ("type 42", text("42")),
            ("type the quick brown fox", text("the quick brown fox")),
            ("type hello and press enter", NeedsEnter),
            ("type hello and hit enter", NeedsEnter),
            ("type thanks then press enter.", NeedsEnter),
            ("please type the report and send it", NeedsEnter),
            ("type that and submit", NeedsEnter),
            ("type what I'm saying", NotTyping),
            ("type what I say", NotTyping),
            ("type", NotTyping),
            ("type  ", NotTyping),
            ("type out the whole story", NotTyping),
            ("type up the notes", NotTyping),
            ("typewriter prices", NotTyping),
            ("write me a poem about the sea", NotTyping),
            ("what is a good typing speed", NotTyping),
            ("open Notepad and type hello", NotTyping),
            ("tell me how to type faster", NotTyping),
            ("I can type fifty words a minute", NotTyping),
            ("send it", NotTyping),
            ("click send", NotTyping),
            ("hello", NotTyping),
        ] {
            assert_eq!(typing_asked(said), read, "{said:?}");
        }
    }

    #[test]
    fn dictate_then_stop() {
        alone(|| {
        clear();
        let _ = after_step("Opened Word.", "Word", Kind::App);
        let start = continue_turn("write what I'm saying", &None, None, true).unwrap();
        assert!(start.contains("Stop Aqua"));
        let stopped = continue_turn("Stop Aqua", &None, None, true).unwrap();
        assert!(stopped.starts_with("Stopped"));
        assert!(continue_turn("hello", &None, None, true).is_none());
        let _ = after_step("Opened Word.", "Word", Kind::App);
        let _ = continue_turn("write what I'm saying", &None, None, true);
        assert!(continue_turn("what's the weather today", &None, None, true).is_none(), "a weather question leaves dictation");
        })
    }
}
