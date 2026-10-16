//! Is a spoken sentence something the person wants done or looked up? What the microphone hears is not all requests: a television,
//! someone else in the room, the assistant's own voice picked up again, and words Whisper invents for noise all arrive as text. With
//! no test for it, the router sent whatever it could not place to the tool agent, which then held the one language model for 2 to 5
//! minutes and ended in an apology (6 of the 25 agent runs in three days of Aqua's own log were not requests, and 5 of the 25 took
//! 205 to 301 seconds).
//!
//! The test is generous about what a request is and strict about where the action word stands. A command begins with its verb
//! ("open Notepad", "could you check my mail", "I want to find my CV"); a sentence that only contains one ("I'm going to play the
//! game") is somebody talking. It only ever takes the agent route away from a sentence that another rule sent there; it never
//! sends anything to the agent.

use crate::laya_validator::{looks_actionable, looks_live_info, IMPERATIVES};

/// Verbs that begin a command, beyond the ones the router already knows. A missed request costs the person a repeat and a wrongly
/// accepted sentence costs what it cost before this test existed, so the list is wide.
const MORE_COMMAND_VERBS: &[&str] = &[
    "read", "list", "show", "run", "execute", "commit", "push", "pull", "fetch", "edit", "create", "remove", "tell", "summarize",
    "summarise", "count", "compare", "scroll", "press", "minimize", "maximize", "switch", "turn", "set", "remind", "take", "save",
    "print", "scan", "ping", "kill", "restart", "apply", "get", "navigate", "browse", "google", "email", "text", "mute", "unmute",
    "pause", "resume", "skip", "increase", "decrease", "raise", "lower", "toggle", "enable", "disable", "connect", "disconnect",
    "shutdown", "lock", "capture", "record", "add", "bring", "give", "make", "select", "enter", "focus", "refresh", "reload",
];

/// Words that may stand before the verb without changing what is meant.
const FILLERS: &[&str] = &["please", "kindly", "hey", "hi", "hello", "okay", "ok", "alright", "now", "just", "also", "well", "um", "uh", "yeah", "yes", "aqua"];

/// Asking for something, or saying what one wants: whatever follows is the thing asked for ("can you hear me", "I need the
/// calculator", "let's check my mail").
const ASKING: &[&[&str]] = &[
    &["can", "you"], &["could", "you"], &["would", "you"], &["will", "you"], &["can", "u"], &["could", "u"],
    &["you", "have", "to"], &["you", "need", "to"], &["you", "should"], &["you", "must"],
    &["i", "want"], &["i", "wanna"], &["i", "need"], &["i", "would", "like"], &["id", "like"],
    &["lets"], &["let", "us"], &["go", "ahead", "and"], &["help", "me"],
];

/// Things on this computer that a question can be about.
const DEVICE_WORDS: &[&str] = &[
    "screen", "window", "windows", "tab", "tabs", "page", "clipboard", "downloads", "inbox", "battery", "disk", "desktop", "taskbar",
    "notepad", "calculator", "chrome", "browser", "file", "files", "folder", "folders", "document", "documents", "email", "emails",
    "mail", "calendar", "notifications", "volume", "wifi", "bluetooth", "screenshot", "app", "apps", "program", "programs", "computer",
    "pc", "laptop", "memory", "cpu", "storage",
];

const QUESTION_STARTS: &[&str] = &["what", "whats", "which", "how", "is", "are", "do", "does", "did", "can", "could", "who", "where", "when", "any"];

/// The words of a piece of speech, lower case, with apostrophes dropped ("let's" is "lets").
fn words_of(piece: &str) -> Vec<String> {
    piece
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '\u{2019}'))
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect()
}

/// The sentence cut into its clauses: at punctuation, and at "and", "then", "so" and "but".
fn clauses(text: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for piece in text.split(['.', '?', '!', ',', ';', ':', '\u{2026}', '\n']) {
        let mut current: Vec<String> = Vec::new();
        for w in words_of(piece) {
            if matches!(w.as_str(), "and" | "then" | "so" | "but") {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            } else {
                current.push(w);
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out
}

/// Does this clause begin with an order or a request, after the politeness in front of it?
fn begins_with_a_command(clause: &[String]) -> bool {
    let words: Vec<&str> = clause.iter().map(String::as_str).skip_while(|w| FILLERS.contains(w)).collect();
    let Some(first) = words.first() else { return false };
    if ASKING.iter().any(|p| words.starts_with(p) && words.len() > p.len()) {
        return true;
    }
    IMPERATIVES.contains(first) || MORE_COMMAND_VERBS.contains(first) || (*first == "look" && matches!(words.get(1), Some(&"up" | &"for" | &"into")))
}

/// What a request by voice (or typed at the orb) may not do: these need a click, a key press or typing in an app or a page, and the
/// guard refuses them for such a request.
const INTERACTIVE_VERBS: &[&str] = &[
    "click", "type", "send", "submit", "post", "buy", "order", "fill", "upload", "attach", "reply", "forward", "subscribe", "tweet", "book",
    "login", "signin", "signup",
];

/// The first command of a request, after politeness and "can you" / "I want to": its verb, and the word after it. "Can you send an
/// email to Bob" gives ("send", "an"); a clause that is not a command gives `None`.
fn command_of(clause: &[String]) -> Option<(String, String)> {
    let mut words: Vec<&str> = clause.iter().map(String::as_str).skip_while(|w| FILLERS.contains(w)).collect();
    let mut asked = false;
    if let Some(p) = ASKING.iter().find(|p| words.starts_with(p) && words.len() > p.len()) {
        words.drain(..p.len());
        asked = true;
    }
    if asked && words.first() == Some(&"to") {
        words.remove(0); // "I want to send ..."
    }
    // "log in", "sign in" and "sign up" are one verb.
    let joined = match (words.first().copied(), words.get(1).copied()) {
        (Some("log" | "sign"), Some("in" | "up")) => Some((format!("{}{}", words[0], words[1]), words.get(2).copied().unwrap_or("").to_string())),
        _ => None,
    };
    if let Some(pair) = joined {
        return Some(pair);
    }
    let verb = *words.first()?;
    (asked || IMPERATIVES.contains(&verb) || MORE_COMMAND_VERBS.contains(&verb) || INTERACTIVE_VERBS.contains(&verb))
        .then(|| (verb.to_string(), words.get(1).copied().unwrap_or("").to_string()))
}

/// Is the first thing this request asks for something that needs a click or typing ("send an email to Bob", "click the Send button",
/// "fill in the form")? A request by voice cannot do that, and planning it only ends in a refusal after minutes of the model's time;
/// the caller answers at once. Only the first command decides: "open Notepad and type hello" starts with something that can be done.
/// "Send me the weather" asks to be told, not to send.
pub fn needs_clicks_or_typing(text: &str) -> bool {
    for clause in clauses(text) {
        let Some((verb, next)) = command_of(&clause) else { continue };
        return INTERACTIVE_VERBS.contains(&verb.as_str()) && !(verb == "send" && matches!(next.as_str(), "me" | "us"));
    }
    false
}

/// Is this sentence plausibly something the person wants done or looked up? (Spoken sentences only: typed text is deliberate.)
pub fn is_request_like(text: &str) -> bool {
    if looks_live_info(text) {
        return true;
    }
    let clauses = clauses(text);
    if clauses.iter().any(|c| begins_with_a_command(c)) {
        return true;
    }
    let words: Vec<&str> = clauses.iter().flatten().map(String::as_str).collect();
    // A fragment that only names something to open ("Notepad.", "YouTube") has lost its verb.
    if words.len() <= 3 && looks_actionable(text) {
        return true;
    }
    // A question about something on this computer ("What's on my screen?", "Is Chrome open?").
    let asks = text.trim_end().ends_with('?') || words.first().is_some_and(|w| QUESTION_STARTS.contains(w));
    asks && words.iter().any(|w| DEVICE_WORDS.contains(w))
}

#[cfg(test)]
mod tests {
    use super::is_request_like;

    /// Every goal that reached the tool agent in three days of Aqua's own log (2026-10-01 to 2026-10-03), and whether it was a
    /// request. The ones that were not took 2 to 5 minutes each.
    const LOGGED: &[(&str, bool)] = &[
        ("open notepad", true),
        ("Open Notepad", true),
        ("Open the calculator and tell me what window is in front", true),
        ("You have to search in my system visual CV and open it for me.", true),
        ("Today is weather. Tell me today is weather.", true),
        ("Tell me today's weather.", true),
        ("Tell me today is weather.", true),
        ("Tell me today is better.", true),
        ("Search my system for Notepad", true),
        ("Search my system for Need for Speed", true),
        ("Open, open.", true),
        ("Open it for a speed.", true),
        ("Can you open a neat voice feed with me for me? Open neat voice feed.", true),
        ("The lady, are you satisfied with your time at the academy so far? Yes, I'm enjoy\u{2026}", false),
        ("Just talking about you. Yeah, naughty, naughty.", false),
        ("I'm going to play the game with the only Thaisur.", false),
        ("I'm going to do a little bit of a video.", false),
        ("I can't understand what are you saying. Please speak properly.", false),
        ("How many persons you are taking right now?", false),
        ("Hi. It's the reason why I'm here today. It's the reason why I'm here today. The \u{2026}", false),
        ("Hello everyone. Hello everyone.", false),
        ("Neat for speed. So I will turn my device.", false),
    ];

    #[test]
    fn the_goals_in_the_log_are_sorted_into_requests_and_room_noise() {
        assert_eq!(LOGGED.len(), 22);
        for (goal, request) in LOGGED {
            assert_eq!(is_request_like(goal), *request, "{goal}");
        }
    }

    #[test]
    fn ordinary_ways_of_asking_are_all_requests() {
        for goal in [
            "What's on my screen?",
            "Check my email",
            "Summarize this page",
            "Take a screenshot",
            "Is Chrome open?",
            "Find the PDF of my electricity bill",
            "Please open the downloads folder",
            "Could you tell me the weather in Pune",
            "I want to open Visual Studio Code",
            "Let's check my mail",
            "I need the calculator",
            "I'd like to see my calendar",
            "Play some music",
            "Look up the capital of France",
            "Notepad.",
            "YouTube",
            "Scroll down",
            "Close this window",
            "Message John that I'm running late",
            "Read the file notes.txt",
            "How much battery is left?",
            "What's the latest news?",
            "Bitcoin price",
            "Remind me at five to call Bob",
            "Yes. Open Notepad.",
            "Okay, go to youtube.com",
            "Help me find my CV",
            "Any new emails?",
            "Where is my resume file?",
            "My downloads folder, open it",
            "Hello open notepad",
            "Is there a file called budget on my desktop?",
        ] {
            assert!(is_request_like(goal), "{goal}");
        }
    }

    #[test]
    fn talk_that_only_contains_an_action_word_is_not_a_request() {
        for chatter in [
            "Thank you very much for your help",
            "That was a really good movie",
            "I think it's going to rain later",
            "My brother is coming to visit tomorrow",
            "Yeah I know, right",
            "No, I don't want that",
            "What are you doing right now",
            "Who are you talking to",
            "Are you listening to me",
            "She said she would call back",
            "We are going to play football later",
            "He started the car and drove away",
            "The game starts at seven",
            "I was typing a letter yesterday",
        ] {
            assert!(!is_request_like(chatter), "{chatter}");
        }
    }
}

#[cfg(test)]
mod interactive_tests {
    use super::needs_clicks_or_typing;

    #[test]
    fn requests_that_are_about_clicking_or_typing_are_recognised() {
        for goal in [
            "Click the Send button",
            "type hello world into the box",
            "Send an email to Bob saying I'll be late",
            "Please send a message to John",
            "Can you send an email to my boss?",
            "I want to send a WhatsApp to Mom",
            "Submit the form",
            "Buy a pair of running shoes",
            "Order a pizza",
            "Log in to Gmail",
            "Sign in to my bank",
            "Fill in the form with my details",
            "Reply to Bob's email",
            "Post this on Twitter",
            "Hey Aqua, click on the first result",
            "Yes. Send the email.",
            "Book a table for two",
        ] {
            assert!(needs_clicks_or_typing(goal), "{goal}");
        }
    }

    #[test]
    fn requests_that_can_be_done_or_that_only_ask_are_not() {
        for goal in [
            "Send me the weather in Pune",
            "What's the weather in Pune?",
            "Open Notepad and type hello",
            "Open the calculator and tell me what window is in front",
            "Search for flights to Delhi",
            "How do I send an email?",
            "Tell me how to click a link",
            "Play some music",
            "Find my CV",
            "Is Chrome open?",
            "Check my email",
            "Read the file notes.txt",
            "Summarize this page",
            "Notepad.",
            "",
        ] {
            assert!(!needs_clicks_or_typing(goal), "{goal}");
        }
    }
}
