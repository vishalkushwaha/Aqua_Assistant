//! Local wake / sleep session commands — handled before llama.cpp.

use chrono::{Local, Timelike};

/// Session-level voice commands handled without the LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommand {
    Wake,
    Sleep,
}

/// Result of inspecting a transcript for a local session command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandHandling {
    /// Speak a predefined reply and update session state.
    Speak {
        command: SessionCommand,
        text: String,
    },
    /// Already awake / already asleep — skip LLM and skip TTS.
    AlreadyInState {
        command: SessionCommand,
    },
    /// Not a wake/sleep command; continue normal conversational processing.
    NotACommand,
}

fn normalize_utterance(text: &str) -> String {
    let lowered: String = text
        .to_lowercase()
        .chars()
        // Apostrophes disappear ("I'm" is "im", "don't" is "dont"); anything else that is not a letter, digit or
        // space separates words.
        .filter(|c| !matches!(c, '\'' | '\u{2019}' | '\u{2018}'))
        .map(|c| {
            if c.is_ascii_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    // Whisper often writes the assistant's name as one of these; they count as the name (whole words only).
    lowered
        .split_whitespace()
        .map(|w| if matches!(w, "acwa" | "akwa" | "aqwa" | "ahkwa" | "aquah" | "aquas") { "aqua" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The request with the wake phrase taken off the front: "Hey Aqua, open Notepad." becomes "open Notepad." The rest stays exactly as
/// it was said (capitals, dots and dashes are part of the names of files and programs). Speech recognition writes a comma after
/// the name ("Hey Aqua, ..."), and every check of "is this a plain command" used to see it as part of the request, so a spoken
/// command that began with the wake phrase was planned by the model for minutes instead of being done at once. The name may also
/// come back as "Aquah" or "Acwa", which count. `wakeup_word` is the phrase set in the settings ("Hey Aqua" by default).
pub fn without_wake_phrase(raw: &str, wakeup_word: &str) -> String {
    /// A word of the text in the form it is compared in: letters and digits only, lower case, the name's spellings as one.
    fn compare_form(word: &str) -> String {
        let w: String = word.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
        if matches!(w.as_str(), "acwa" | "akwa" | "aqwa" | "ahkwa" | "aquah" | "aquas") { "aqua".to_string() } else { w }
    }
    let mut phrases: Vec<Vec<String>> = vec![vec!["aqua".into()]];
    for greeting in ["hey", "hi", "hello", "okay", "ok"] {
        phrases.push(vec![greeting.into(), "aqua".into()]);
    }
    let custom: Vec<String> = wakeup_word.split_whitespace().map(compare_form).filter(|w| !w.is_empty()).collect();
    if !custom.is_empty() {
        phrases.push(custom);
    }
    // The longest phrase first, so "hey aqua" is taken as one and not as "hey" followed by a request.
    phrases.sort_by_key(|p| std::cmp::Reverse(p.len()));

    let mut rest = raw.trim_start();
    loop {
        // The first words of what is left, each with where it ends in `rest`.
        let mut words: Vec<(String, usize)> = Vec::new();
        let mut at = 0usize;
        while words.len() < 4 && at < rest.len() {
            let tail = &rest[at..];
            let skipped = tail.len() - tail.trim_start().len();
            let word_start = at + skipped;
            if word_start >= rest.len() {
                break;
            }
            let word_len = rest[word_start..].find(char::is_whitespace).unwrap_or(rest.len() - word_start);
            let end = word_start + word_len;
            let form = compare_form(&rest[word_start..end]);
            if !form.is_empty() {
                words.push((form, end));
            }
            at = end;
        }
        let taken = phrases.iter().find(|p| p.len() <= words.len() && words.iter().zip(p.iter()).all(|((w, _), want)| w == want)).map(|p| words[p.len() - 1].1);
        let Some(end) = taken else { break };
        rest = rest[end..].trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | ':' | ';' | '!' | '?' | '-' | '\u{2013}' | '\u{2014}'));
    }
    rest.to_string()
}

fn strip_wake_prefixes(normalized: &str, wakeup_word: &str) -> String {
    let mut rest = normalized.to_string();
    let mut prefixes: Vec<String> = vec![
        "hey aqua".into(),
        "hi aqua".into(),
        "okay aqua".into(),
        "ok aqua".into(),
        "aqua".into(),
    ];
    let clean_wake = normalize_utterance(wakeup_word);
    if !clean_wake.is_empty() && !prefixes.iter().any(|p| p == &clean_wake) {
        prefixes.insert(0, clean_wake);
    }
    // Longest first so "hey aqua" wins over "aqua".
    prefixes.sort_by_key(|p| std::cmp::Reverse(p.len()));

    loop {
        let before = rest.clone();
        for prefix in &prefixes {
            if rest == *prefix {
                rest.clear();
                break;
            }
            let with_space = format!("{prefix} ");
            if rest.starts_with(&with_space) {
                rest = rest[with_space.len()..].trim().to_string();
                break;
            }
        }
        if rest == before {
            break;
        }
    }
    rest
}

fn is_wake_command_phrase(phrase: &str) -> bool {
    matches!(
        phrase,
        "wake up"
            | "wake me up"
            | "wakeup"
            | "wake"
            | "good morning"
            | "good afternoon"
            | "good evening"
            | "im awake"
            | "i am awake"
    )
}

fn is_sleep_command_phrase(phrase: &str) -> bool {
    matches!(
        phrase,
        "sleep"
            | "go to sleep"
            | "go to bed"
            | "good night"
            | "goodnight"
            | "im going to sleep"
            | "i am going to sleep"
            | "time to sleep"
            | "bye"
            | "bye bye"
            | "goodbye"
            | "good bye"
    )
}

/// Classify a transcript as wake / sleep when it is clearly a command, not casual talk.
pub fn classify_session_command(text: &str, wakeup_word: &str) -> Option<SessionCommand> {
    let normalized = normalize_utterance(text);
    if normalized.is_empty() {
        return None;
    }

    // Whole utterance is a command (e.g. "wake up", "go to sleep").
    if is_wake_command_phrase(&normalized) {
        return Some(SessionCommand::Wake);
    }
    if is_sleep_command_phrase(&normalized) {
        return Some(SessionCommand::Sleep);
    }

    // "Hey Aqua, wake up" / "Aqua go to sleep" after stripping wake prefixes.
    let stripped = strip_wake_prefixes(&normalized, wakeup_word);
    if stripped.is_empty() {
        // Bare wake word alone is treated as wake.
        return Some(SessionCommand::Wake);
    }
    if is_wake_command_phrase(&stripped) {
        return Some(SessionCommand::Wake);
    }
    if is_sleep_command_phrase(&stripped) {
        return Some(SessionCommand::Sleep);
    }

    None
}

/// Local-time daypart for greetings / farewells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayPart {
    Morning,
    Afternoon,
    Evening,
    Night,
}

pub fn day_part_from_hour(hour: u32) -> DayPart {
    match hour {
        5..=11 => DayPart::Morning,
        12..=16 => DayPart::Afternoon,
        17..=20 => DayPart::Evening,
        _ => DayPart::Night, // 21–4
    }
}

pub fn wake_greeting_for_day_part(part: DayPart) -> &'static str {
    match part {
        DayPart::Morning => "Good morning, Master.",
        DayPart::Afternoon => "Good afternoon, Master.",
        DayPart::Evening => "Good evening, Master.",
        DayPart::Night => "Welcome back, Master.",
    }
}

pub fn sleep_farewell_for_day_part(part: DayPart) -> &'static str {
    match part {
        DayPart::Evening | DayPart::Night => "Good night, Master.",
        DayPart::Morning | DayPart::Afternoon => "Goodbye, Master. See you later.",
    }
}

pub fn wake_greeting_now() -> String {
    let hour = Local::now().hour();
    wake_greeting_for_day_part(day_part_from_hour(hour)).to_string()
}

pub fn sleep_farewell_now() -> String {
    let hour = Local::now().hour();
    sleep_farewell_for_day_part(day_part_from_hour(hour)).to_string()
}

/// How long a voice session stays open without anyone speaking to it.
pub const SESSION_IDLE_SECS: i64 = 60;

/// Whether the assistant is awake. The server decides this, never the caller: a request that merely claims to be
/// inside a session must not skip the wake word. A wake command opens it, a sleep command closes it, and every
/// accepted turn keeps it open for `SESSION_IDLE_SECS` more.
#[derive(Debug, Default)]
pub struct VoiceSession {
    until: std::sync::atomic::AtomicI64,
}

impl VoiceSession {
    pub fn is_active(&self, now: i64) -> bool {
        now < self.until.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn open(&self, now: i64) {
        self.until.store(now + SESSION_IDLE_SECS, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn close(&self) {
        self.until.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    /// Keep an open session open; does nothing for a closed one.
    pub fn touch(&self, now: i64) {
        if self.is_active(now) {
            self.open(now);
        }
    }

    /// Update the session after a turn. `heard` is the transcript; `accepted` is false when the turn was ignored
    /// (no speech, no wake word) or refused (speaker not verified), which must change nothing.
    ///
    /// An accepted turn got through the wake gate, so it either carried the wake word or came inside a session:
    /// either way the person is talking to Aqua, and the session is open for `SESSION_IDLE_SECS` from now. This
    /// does not depend on the session still being open when the answer is ready, so a slow answer (a minute of
    /// model time) cannot end the conversation it belongs to. Only a sleep command closes it.
    pub fn after_turn(&self, heard: &str, wakeup_word: &str, accepted: bool, now: i64) {
        if !accepted {
            return;
        }
        match classify_session_command(heard, wakeup_word) {
            Some(SessionCommand::Sleep) => {
                self.close();
                // Going to sleep ends the conversation: what was said before is not carried into the next one.
                crate::conversation::clear();
            }
            Some(SessionCommand::Wake) | None => self.open(now),
        }
    }
}

/// The one voice session of this assistant (it has a single owner).
pub fn voice_session() -> &'static VoiceSession {
    static SESSION: std::sync::OnceLock<VoiceSession> = std::sync::OnceLock::new();
    SESSION.get_or_init(VoiceSession::default)
}

/// A turn that produced no real answer: nothing heard, no wake word, or the speaker was refused.
pub fn turn_was_accepted(assistant_text: &str) -> bool {
    !(assistant_text.starts_with("IGNORED") || assistant_text.starts_with("ACCESS DENIED") || assistant_text.starts_with("Pipeline error"))
}

/// Resolve how to handle a transcript relative to the current session state.
pub fn resolve_session_command(
    text: &str,
    wakeup_word: &str,
    active_session: bool,
) -> CommandHandling {
    match classify_session_command(text, wakeup_word) {
        Some(SessionCommand::Wake) => {
            if active_session {
                CommandHandling::AlreadyInState {
                    command: SessionCommand::Wake,
                }
            } else {
                CommandHandling::Speak {
                    command: SessionCommand::Wake,
                    text: wake_greeting_now(),
                }
            }
        }
        Some(SessionCommand::Sleep) => {
            if !active_session {
                CommandHandling::AlreadyInState {
                    command: SessionCommand::Sleep,
                }
            } else {
                CommandHandling::Speak {
                    command: SessionCommand::Sleep,
                    text: sleep_farewell_now(),
                }
            }
        }
        None => CommandHandling::NotACommand,
    }
}

/// Whether the utterance should pass the wake-word gate (includes local wake commands).
pub fn passes_wake_gate(text: &str, wakeup_word: &str, active_session: bool) -> bool {
    if active_session {
        return true;
    }
    let normalized = normalize_utterance(text);
    let clean_wake = normalize_utterance(wakeup_word);
    // Whole words only: "aquarium" or "they aquatic" must not wake the assistant.
    let padded = format!(" {normalized} ");
    if !clean_wake.is_empty() && padded.contains(&format!(" {clean_wake} ")) {
        return true;
    }
    // Only the assistant's name wakes it. "Good morning" or "wake up" said in the room (a television, a colleague) must not
    // open a session in which every later sentence skips this gate. Once awake, those phrases still work as commands.
    padded.contains(" aqua ")
}

/// Was the orb's wake phrase said anywhere in this sentence? The assistant's name ("Hey Aqua", "Aqua, ...") counts, and so
/// does "wake up" said to the orb itself: the orb is a window the person opened and chose to talk to, which a television
/// or a colleague saying "good morning" in the room is not (that is why the server's own wake gate takes only the name).
/// Whole words only: "aquarium" and "wakeup" inside another word do not count.
pub fn wake_phrase_heard(text: &str, wakeup_word: &str) -> bool {
    if passes_wake_gate(text, wakeup_word, false) {
        return true;
    }
    let padded = format!(" {} ", normalize_utterance(text));
    padded.contains(" wake up ") || padded.contains(" wakeup ")
}

/// Is this an order to stop whatever Aqua is doing right now (thinking, working or speaking)? The whole utterance has to be
/// the order, with at most the name in front or behind ("Aqua, stop", "stop Aqua") and a "please" around it, so a sentence
/// that only contains the word ("stop signs are red", "don't stop the music") never counts.
pub fn is_stop_command(text: &str, wakeup_word: &str) -> bool {
    let normalized = normalize_utterance(text);
    if normalized.is_empty() {
        return false;
    }
    let mut phrase = strip_wake_prefixes(&normalized, wakeup_word);
    if let Some(rest) = phrase.strip_suffix(" aqua") {
        phrase = rest.to_string();
    }
    let phrase = phrase.split_whitespace().filter(|w| *w != "please").collect::<Vec<_>>().join(" ");
    matches!(
        phrase.as_str(),
        "stop"
            | "stop it"
            | "stop that"
            | "stop now"
            | "stop talking"
            | "stop speaking"
            | "stop thinking"
            | "stop right now"
            | "cancel"
            | "cancel that"
            | "cancel it"
            | "cancel the task"
            | "cancel this"
            | "be quiet"
            | "quiet"
            | "enough"
            | "that is enough"
            | "thats enough"
            | "never mind"
            | "nevermind"
            | "shut up"
    )
}

/// What is done with a line said while a task runs (card OA10-03, UX-11): added to the task at its next step (`Steer`), or kept for after the task and then started as a task of its own
/// (`Queue`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Steer,
    Queue,
}

impl Delivery {
    /// The word a request says (`steer` or `queue`).
    pub fn parse(word: &str) -> Option<Delivery> {
        match word.trim().to_ascii_lowercase().as_str() {
            "steer" => Some(Delivery::Steer),
            "queue" => Some(Delivery::Queue),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Steer => "steer",
            Delivery::Queue => "queue",
        }
    }
}

/// The words that begin a line that goes on from what is being done, and what each means. "also", "and also", "one more thing", "wait", "actually" and "instead" change the task that is
/// going: they are added to it. "then", "and then", "after that", "when you're done" and "next" are for after it: they start once it has ended. The longer words come first, and the
/// apostrophe is left out of what is compared ("you're" is "youre").
const STEER_CUES: &[(&str, Delivery)] = &[
    ("one more thing", Delivery::Steer),
    ("and also", Delivery::Steer),
    ("also", Delivery::Steer),
    ("wait", Delivery::Steer),
    ("actually", Delivery::Steer),
    ("instead", Delivery::Steer),
    ("and then", Delivery::Queue),
    ("then", Delivery::Queue),
    ("after that", Delivery::Queue),
    ("when you are done", Delivery::Queue),
    ("when youre done", Delivery::Queue),
    ("when you have finished", Delivery::Queue),
    ("when youre finished", Delivery::Queue),
    ("next", Delivery::Queue),
];

/// Characters that may stand between a cue and what follows it.
fn is_cue_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | ':' | ';' | '.' | '-')
}

/// Does this sentence continue the task that is going (card OA10-03, UX-11)? `Some((delivery, line))` when it begins with one of the cues of [`STEER_CUES`] and says something after it:
/// the line is what is kept: the whole sentence for a steer ("also add tests": it reads as it was said, with its cue), and what comes after the cue for a queued one ("then run clippy"
/// starts a task that says "run clippy"). `None` for everything else, which is a new request as it always was. A yes, a no, an approval word or a stop is never a cue: nothing said this
/// way answers a question of the host (that is the app's alone), and a stop is handled before this is asked. `wakeup_word` is the wake phrase of the settings, which may come first.
pub fn steer_cue(text: &str, wakeup_word: &str) -> Option<(Delivery, String)> {
    let said = without_wake_phrase(text, wakeup_word);
    let said = said.trim();
    if said.is_empty() || is_stop_command(said, wakeup_word) {
        return None;
    }
    // Without apostrophes, so "when you're done" is found; the ASCII-only lower case has the length of the text it came from, so what follows a cue is cut from the original.
    let plain: String = said.chars().filter(|c| !matches!(c, '\'' | '\u{2019}' | '\u{2018}')).collect();
    let lowered = plain.to_ascii_lowercase();
    for (cue, delivery) in STEER_CUES {
        let Some(rest) = lowered.strip_prefix(cue) else { continue };
        // A whole word: "alsop", "waiting" and "nextdoor" are not cues.
        if !rest.starts_with(is_cue_separator) {
            continue;
        }
        let after = plain[cue.len()..].trim_start_matches(is_cue_separator).trim();
        if after.is_empty() {
            return None;
        }
        return Some(match delivery {
            Delivery::Steer => (Delivery::Steer, said.to_string()),
            Delivery::Queue => (Delivery::Queue, after.to_string()),
        });
    }
    None
}

/// What a person says to hear or look at the changes of a coding run, and to take them back (card OA10-06, UX-16). Nothing here changes a file, and nothing here answers a question of the host:
/// reading is reading, "show me the diff" and "undo the last run" only put a card on the screen, and the person presses the button there (`coding_voice`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingCommand {
    /// "read the diff": what the last run changed, in a few sentences.
    ReadDiff,
    /// "line by line": the change that was read last, every changed line with its number.
    LineByLine,
    NextChange,
    PreviousChange,
    /// "repeat": the last thing that was read.
    Repeat,
    /// "show me the diff": the changes on the screen.
    ShowDiff,
    /// "undo the last run": a card on the screen with the undo, pressed by the person.
    UndoLastRun,
}

impl CodingCommand {
    /// The word a log or a test uses for it.
    pub fn as_str(self) -> &'static str {
        match self {
            CodingCommand::ReadDiff => "read_diff",
            CodingCommand::LineByLine => "line_by_line",
            CodingCommand::NextChange => "next_change",
            CodingCommand::PreviousChange => "previous_change",
            CodingCommand::Repeat => "repeat",
            CodingCommand::ShowDiff => "show_diff",
            CodingCommand::UndoLastRun => "undo_last_run",
        }
    }
}

/// The whole of what is said, as the words are compared (lower case, no punctuation, no apostrophes, the wake phrase and the polite words around it taken off), and what it means. The whole
/// sentence has to be the command: "read the diff of the movie reviews" is a request, and so is "next" alone (that one goes on as a line for after the task, see [`steer_cue`]).
const CODING_PHRASES: &[(&str, CodingCommand)] = &[
    ("read the diff", CodingCommand::ReadDiff),
    ("read me the diff", CodingCommand::ReadDiff),
    ("read the diff out loud", CodingCommand::ReadDiff),
    ("read the changes", CodingCommand::ReadDiff),
    ("read me the changes", CodingCommand::ReadDiff),
    ("read back the changes", CodingCommand::ReadDiff),
    ("read it back", CodingCommand::ReadDiff),
    ("what did you change", CodingCommand::ReadDiff),
    ("what did you just change", CodingCommand::ReadDiff),
    ("what have you changed", CodingCommand::ReadDiff),
    ("tell me what you changed", CodingCommand::ReadDiff),
    ("line by line", CodingCommand::LineByLine),
    ("read it line by line", CodingCommand::LineByLine),
    ("read the diff line by line", CodingCommand::LineByLine),
    ("go line by line", CodingCommand::LineByLine),
    ("spell it out", CodingCommand::LineByLine),
    ("spell out the change", CodingCommand::LineByLine),
    ("next change", CodingCommand::NextChange),
    ("the next change", CodingCommand::NextChange),
    ("next one", CodingCommand::NextChange),
    ("go to the next change", CodingCommand::NextChange),
    ("read the next change", CodingCommand::NextChange),
    ("previous change", CodingCommand::PreviousChange),
    ("the previous change", CodingCommand::PreviousChange),
    ("go back one change", CodingCommand::PreviousChange),
    ("go to the previous change", CodingCommand::PreviousChange),
    ("read the previous change", CodingCommand::PreviousChange),
    ("repeat", CodingCommand::Repeat),
    ("repeat that", CodingCommand::Repeat),
    ("say that again", CodingCommand::Repeat),
    ("say it again", CodingCommand::Repeat),
    ("read that again", CodingCommand::Repeat),
    ("show me the diff", CodingCommand::ShowDiff),
    ("show the diff", CodingCommand::ShowDiff),
    ("show me the changes", CodingCommand::ShowDiff),
    ("show the changes", CodingCommand::ShowDiff),
    ("open the diff", CodingCommand::ShowDiff),
    ("undo the last run", CodingCommand::UndoLastRun),
    ("undo last run", CodingCommand::UndoLastRun),
    ("undo the run", CodingCommand::UndoLastRun),
    ("undo the changes", CodingCommand::UndoLastRun),
    ("undo what you did", CodingCommand::UndoLastRun),
    ("undo that run", CodingCommand::UndoLastRun),
    ("take back the changes", CodingCommand::UndoLastRun),
    ("revert the last run", CodingCommand::UndoLastRun),
];

/// Is this a command of [`CodingCommand`]: the whole sentence, with the wake phrase and a "please" or "can you" around it allowed. Anything that is not exactly one of the phrases is `None`
/// and goes on as it would have. Nothing is decided here about whether there is anything to read; `coding_voice` does that.
pub fn coding_command(text: &str, wakeup_word: &str) -> Option<CodingCommand> {
    let normalized = normalize_utterance(text);
    if normalized.is_empty() {
        return None;
    }
    let mut phrase = strip_wake_prefixes(&normalized, wakeup_word);
    // Polite words in front, in any order and as many as there are, and at the end.
    loop {
        let mut changed = false;
        for polite in ["please ", "can you ", "could you ", "would you ", "will you ", "go ahead and ", "now "] {
            if let Some(rest) = phrase.strip_prefix(polite) {
                phrase = rest.to_string();
                changed = true;
            }
        }
        for polite in [" please", " now", " for me"] {
            if let Some(rest) = phrase.strip_suffix(polite) {
                phrase = rest.to_string();
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    CODING_PHRASES.iter().find(|(p, _)| *p == phrase).map(|(_, c)| *c)
}

pub fn session_action_label(command: SessionCommand) -> &'static str {
    match command {
        SessionCommand::Wake => "wake",
        SessionCommand::Sleep => "sleep",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_wake_variants() {
        for phrase in [
            "wake up",
            "Wake Up!",
            "wake me up",
            "good morning",
            "Hey Aqua, wake up",
            "aqua wake me up",
        ] {
            assert_eq!(
                classify_session_command(phrase, "Hey Aqua"),
                Some(SessionCommand::Wake),
                "failed for {phrase}"
            );
        }
    }

    #[test]
    fn detects_sleep_variants() {
        for phrase in [
            "sleep",
            "go to sleep",
            "Go to bed.",
            "good night",
            "Hey Aqua go to sleep",
            "aqua sleep",
            "bye",
            "Bye!",
            "goodbye",
            "Aqua bye",
        ] {
            assert_eq!(
                classify_session_command(phrase, "Hey Aqua"),
                Some(SessionCommand::Sleep),
                "failed for {phrase}"
            );
        }
    }

    #[test]
    fn ignores_casual_sleep_talk() {
        for phrase in [
            "I didn't sleep well",
            "I need more sleep tonight",
            "tell me about sleep cycles",
            "why do people sleep",
            "sleep is important",
        ] {
            assert_eq!(
                classify_session_command(phrase, "Hey Aqua"),
                None,
                "false positive for {phrase}"
            );
        }
    }

    #[test]
    fn already_awake_skips_repeat() {
        let h = resolve_session_command("wake up", "Hey Aqua", true);
        assert!(matches!(
            h,
            CommandHandling::AlreadyInState {
                command: SessionCommand::Wake
            }
        ));
    }

    #[test]
    fn already_asleep_skips_repeat() {
        let h = resolve_session_command("sleep", "Hey Aqua", false);
        assert!(matches!(
            h,
            CommandHandling::AlreadyInState {
                command: SessionCommand::Sleep
            }
        ));
    }

    #[test]
    fn wake_while_asleep_speaks_greeting() {
        let h = resolve_session_command("wake up", "Hey Aqua", false);
        assert!(matches!(
            h,
            CommandHandling::Speak {
                command: SessionCommand::Wake,
                ..
            }
        ));
    }

    #[test]
    fn sleep_while_awake_speaks_farewell() {
        let h = resolve_session_command("go to sleep", "Hey Aqua", true);
        assert!(matches!(
            h,
            CommandHandling::Speak {
                command: SessionCommand::Sleep,
                ..
            }
        ));
    }

    #[test]
    fn day_part_greetings() {
        assert_eq!(
            wake_greeting_for_day_part(DayPart::Morning),
            "Good morning, Master."
        );
        assert_eq!(
            wake_greeting_for_day_part(DayPart::Night),
            "Welcome back, Master."
        );
        assert_eq!(
            sleep_farewell_for_day_part(DayPart::Night),
            "Good night, Master."
        );
        assert_eq!(
            sleep_farewell_for_day_part(DayPart::Afternoon),
            "Goodbye, Master. See you later."
        );
    }

    #[test]
    fn the_wake_word_must_be_a_whole_word() {
        for talk in ["I bought an aquarium", "aquatic life is great", "aquaman is on tonight", "equator aquarius"] {
            assert!(!passes_wake_gate(talk, "Hey Aqua", false), "{talk}");
        }
        for talk in ["Aqua, what time is it", "hey aqua what's up", "what time is it, Aqua?", "AQUA"] {
            assert!(passes_wake_gate(talk, "Hey Aqua", false), "{talk}");
        }
    }

    #[test]
    fn common_mishearings_of_the_name_still_count_as_the_name() {
        assert_eq!(classify_session_command("ACWA go to sleep.", "Hey Aqua"), Some(SessionCommand::Sleep));
        assert!(passes_wake_gate("Akwa, what time is it?", "Hey Aqua", false));
        assert!(!passes_wake_gate("acwafina is a rapper", "Hey Aqua", false), "only whole words");
    }

    #[test]
    fn a_session_opens_on_wake_closes_on_sleep_and_times_out() {
        let s = VoiceSession::default();
        assert!(!s.is_active(1000));
        s.after_turn("hey aqua wake up", "Hey Aqua", true, 1000);
        assert!(s.is_active(1000 + SESSION_IDLE_SECS - 1));
        assert!(!s.is_active(1000 + SESSION_IDLE_SECS), "an idle session closes by itself");
        s.after_turn("good morning", "Hey Aqua", true, 2000);
        s.after_turn("what is the weather", "Hey Aqua", true, 2030);
        assert!(s.is_active(2030 + SESSION_IDLE_SECS - 1), "talking keeps it open");
        s.after_turn("go to sleep", "Hey Aqua", true, 2040);
        assert!(!s.is_active(2041));
    }

    #[test]
    fn a_question_that_carried_the_wake_word_opens_the_session_and_a_slow_answer_cannot_end_it() {
        let s = VoiceSession::default();
        // "Hey Aqua, what is the weather?" was accepted (it passed the wake gate): the follow-up needs no wake word.
        s.after_turn("Hey Aqua, what is the weather", "Hey Aqua", true, 100);
        assert!(s.is_active(100 + SESSION_IDLE_SECS - 1));
        // The session had lapsed while a long answer was being made; the turn that was accepted reopens it.
        s.after_turn("and tomorrow", "Hey Aqua", true, 100 + 3 * SESSION_IDLE_SECS);
        assert!(s.is_active(100 + 3 * SESSION_IDLE_SECS + 1), "a turn that took longer than the idle time still keeps its conversation");
    }

    #[test]
    fn apostrophes_do_not_break_the_command_phrases() {
        assert_eq!(classify_session_command("I'm going to sleep.", "Hey Aqua"), Some(SessionCommand::Sleep));
        assert_eq!(classify_session_command("I\u{2019}m awake", "Hey Aqua"), Some(SessionCommand::Wake));
        assert_eq!(classify_session_command("Hey Aqua, I'm going to sleep", "Hey Aqua"), Some(SessionCommand::Sleep));
    }

    #[test]
    fn ignored_or_refused_turns_change_nothing() {
        let s = VoiceSession::default();
        s.open(100);
        s.after_turn("go to sleep", "Hey Aqua", false, 150);
        assert!(s.is_active(151), "a refused speaker cannot close it either");
        assert!(!turn_was_accepted("IGNORED: Wake-word not detected.") && !turn_was_accepted("ACCESS DENIED: x") && turn_was_accepted("It is noon."));
    }

    #[test]
    fn the_orb_wakes_on_the_name_or_on_wake_up_anywhere_in_a_sentence_but_not_on_other_talk() {
        for yes in ["Hey Aqua", "hey aqua, what time is it", "OK Aqua", "so... wake up now please", "Wake up!", "wakeup", "Akwa, hello", "Hey Aqua's.", "hey aqua's"] {
            assert!(wake_phrase_heard(yes, "Hey Aqua"), "{yes}");
        }
        for no in ["good morning", "what is the weather", "the aquarium is open", "he woke up late", "wake me up at six", "", "thank you"] {
            assert!(!wake_phrase_heard(no, "Hey Aqua"), "{no}");
        }
    }

    #[test]
    fn a_greeting_alone_does_not_wake_the_assistant_but_the_name_does() {
        for bare in ["good morning", "wake up", "i am awake", "good evening"] {
            assert!(!passes_wake_gate(bare, "Hey Aqua", false), "{bare}");
            assert!(passes_wake_gate(&format!("Hey Aqua, {bare}"), "Hey Aqua", false), "with the name: {bare}");
        }
        assert!(!passes_wake_gate("what's the weather", "Hey Aqua", false));
        assert!(passes_wake_gate("what's the weather", "Hey Aqua", true));
    }
}

#[cfg(test)]
mod stop_tests {
    use super::is_stop_command;

    #[test]
    fn the_whole_utterance_has_to_be_the_order() {
        for yes in ["stop", "Stop.", "Stop!", "stop it", "Stop talking", "Aqua, stop", "Hey Aqua stop", "stop Aqua", "please stop", "Stop, please", "cancel that", "Never mind.", "be quiet", "That's enough", "okay aqua stop thinking"] {
            assert!(is_stop_command(yes, "Hey Aqua"), "{yes}");
        }
        for no in ["", "stop signs are red", "don't stop the music", "can you stop the timer in ten minutes", "what does stop mean", "bus stop", "go to sleep", "tell me about cancel culture"] {
            assert!(!is_stop_command(no, "Hey Aqua"), "{no}");
        }
    }
}

#[cfg(test)]
mod without_wake_phrase_tests {
    use super::{steer_cue, without_wake_phrase, Delivery};

    #[test]
    fn the_wake_phrase_comes_off_the_front_with_its_comma_and_the_rest_is_left_alone() {
        let cut = |s: &str| without_wake_phrase(s, "Hey Aqua");
        assert_eq!(cut("Hey Aqua, open Notepad."), "open Notepad.");
        assert_eq!(cut("hey aqua open notepad"), "open notepad");
        assert_eq!(cut("Hey, Aqua. Open Vishal CV.pdf please"), "Open Vishal CV.pdf please", "capitals, dots and names stay");
        assert_eq!(cut("Okay Aqua: what time is it?"), "what time is it?");
        assert_eq!(cut("Aqua, hey Aqua, play some music"), "play some music", "said twice");
        assert_eq!(cut("  Hello Aqua - search my system for the report"), "search my system for the report");
        // The name as Whisper may write it.
        assert_eq!(cut("Hey Aquah, open Chrome"), "open Chrome");
        assert_eq!(cut("Acwa, open Chrome"), "open Chrome");
        // Only the wake phrase: nothing is left.
        assert_eq!(cut("Hey Aqua"), "");
        assert_eq!(cut("Hey Aqua."), "");
    }

    #[test]
    fn text_that_only_looks_like_the_wake_phrase_is_left_alone() {
        let cut = |s: &str| without_wake_phrase(s, "Hey Aqua");
        for text in ["Open the Aqua folder", "aquarium opening hours", "Aquatic plants for a small pond", "Hey there, open Notepad", "Say hey Aqua to Bob", "What does aqua mean"] {
            assert_eq!(cut(text), text, "{text}");
        }
    }

    #[test]
    fn a_wake_phrase_chosen_in_the_settings_comes_off_too() {
        assert_eq!(without_wake_phrase("Hey Jarvis, open Notepad", "Hey Jarvis"), "open Notepad");
        assert_eq!(without_wake_phrase("Computer, what time is it", "Computer"), "what time is it");
        assert_eq!(without_wake_phrase("Hey Aqua, open Notepad", "Hey Jarvis"), "open Notepad", "the default phrase still counts");
        assert_eq!(without_wake_phrase("open Notepad", "Hey Jarvis"), "open Notepad");
    }

    /// Card OA10-03 (UX-11): which sentences go on from the task that is running, and as what.
    #[test]
    fn cue_table() {
        let cue = |s: &str| steer_cue(s, "Hey Aqua");
        let steer = |s: &str| Some((Delivery::Steer, s.to_string()));
        let queue = |s: &str| Some((Delivery::Queue, s.to_string()));
        // Said to change the task that is going: kept whole, cue and all.
        for line in ["also add tests", "Also add tests for the parser", "and also read b.rs", "One more thing: use tabs", "wait, use b.rs instead", "Actually make it shorter", "instead, edit config.toml"] {
            assert_eq!(cue(line), steer(line), "{line}");
        }
        // Said for after it: only what follows the cue is kept, as it was written.
        assert_eq!(cue("then run clippy"), queue("run clippy"));
        assert_eq!(cue("Then, run Cargo Clippy."), queue("run Cargo Clippy."));
        assert_eq!(cue("and then commit the change"), queue("commit the change"));
        assert_eq!(cue("after that, open README.md"), queue("open README.md"));
        assert_eq!(cue("When you're done, run the tests"), queue("run the tests"));
        assert_eq!(cue("when you\u{2019}re done run the tests"), queue("run the tests"), "a curly apostrophe is the same");
        assert_eq!(cue("next: write the changelog"), queue("write the changelog"));
        // The wake phrase may come first.
        assert_eq!(cue("Hey Aqua, then run clippy"), queue("run clippy"));
        assert_eq!(cue("Hey Aqua, also add tests"), steer("also add tests"));
        // Everything else is a new request, as it always was: no cue, a cue that is only the start of a longer word, and a cue with nothing after it.
        for line in ["open notepad", "what is the weather", "alsop is a name", "waiting for the build", "nextdoor", "thenceforth", "also", "then", "wait.", "", "   "] {
            assert_eq!(cue(line), None, "{line:?}");
        }
        // A stop is a stop, never a cue; and an answer to a question is never one: nothing said this way approves or refuses anything.
        for line in ["stop", "Aqua, stop", "cancel that", "yes", "no", "yes, approve it", "approve", "deny", "go ahead", "okay do it", "sure"] {
            assert_eq!(cue(line), None, "{line:?}");
        }
        assert_eq!((Delivery::parse(" Steer "), Delivery::parse("QUEUE"), Delivery::parse("now")), (Some(Delivery::Steer), Some(Delivery::Queue), None));
        assert_eq!((Delivery::Steer.as_str(), Delivery::Queue.as_str()), ("steer", "queue"));
    }
}

#[cfg(test)]
mod coding_command_tests {
    use super::{coding_command, CodingCommand, CODING_PHRASES};

    /// Card OA10-06 (UX-16): the grammar of the words that read, show and take back what a coding run changed.
    #[test]
    fn grammar_table() {
        let cmd = |s: &str| coding_command(s, "Hey Aqua");
        for (said, want) in [
            ("read the diff", CodingCommand::ReadDiff),
            ("Read the diff.", CodingCommand::ReadDiff),
            ("Hey Aqua, read me the diff", CodingCommand::ReadDiff),
            ("please read the changes", CodingCommand::ReadDiff),
            ("Can you read it back please?", CodingCommand::ReadDiff),
            ("What did you change?", CodingCommand::ReadDiff),
            ("what did you just change", CodingCommand::ReadDiff),
            ("line by line", CodingCommand::LineByLine),
            ("Read it line by line.", CodingCommand::LineByLine),
            ("aqua, spell it out", CodingCommand::LineByLine),
            ("next change", CodingCommand::NextChange),
            ("The next change.", CodingCommand::NextChange),
            ("go to the next change", CodingCommand::NextChange),
            ("previous change", CodingCommand::PreviousChange),
            ("go back one change", CodingCommand::PreviousChange),
            ("repeat", CodingCommand::Repeat),
            ("Repeat that.", CodingCommand::Repeat),
            ("say that again", CodingCommand::Repeat),
            ("show me the diff", CodingCommand::ShowDiff),
            ("Show me the changes!", CodingCommand::ShowDiff),
            ("undo the last run", CodingCommand::UndoLastRun),
            ("Hey Aqua undo the last run.", CodingCommand::UndoLastRun),
            ("could you undo the last run for me", CodingCommand::UndoLastRun),
            ("take back the changes", CodingCommand::UndoLastRun),
        ] {
            assert_eq!(cmd(said), Some(want), "{said}");
        }
        // The whole sentence has to be the command: anything more is a request of its own, and what is not one of the phrases is not a command.
        for not in [
            "",
            "next",
            "then",
            "read the diff of the movie reviews",
            "read the diff and then undo it",
            "undo",
            "undo that",
            "what did you change in my document about the weather",
            "show me the diff between the two documents on my desktop",
            "repeat after me",
            "can you repeat the question",
            "next change in the weather",
            "line by line is how I read",
            "line",
            "undo the last run and delete the folder",
            "read the file",
        ] {
            assert_eq!(cmd(not), None, "{not:?}");
        }
    }

    /// Nothing said this way answers a question of the host or approves anything: the words of an approval are never a coding command, and no phrase of the table has one in it.
    #[test]
    fn an_approval_is_never_a_coding_command() {
        let cmd = |s: &str| coding_command(s, "Hey Aqua");
        for approval in ["yes", "no", "approve", "approve it", "deny", "go ahead", "do it", "okay do it", "sure", "yes undo the last run", "approve the undo", "confirm", "yes, please undo the last run"] {
            assert_eq!(cmd(approval), None, "{approval:?}");
        }
        for (phrase, _) in CODING_PHRASES {
            for word in ["approve", "confirm", "deny", "allow", "yes", "delete"] {
                assert!(!phrase.split_whitespace().any(|w| w == word), "{phrase}: {word}");
            }
        }
        // The names are stable: a log and a test use them.
        assert_eq!(CodingCommand::UndoLastRun.as_str(), "undo_last_run");
        assert_eq!(CODING_PHRASES.iter().map(|(_, c)| c.as_str()).collect::<std::collections::BTreeSet<_>>().len(), 7, "every command has a phrase");
    }
}