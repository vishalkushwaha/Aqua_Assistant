use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

const MODEL_CANDIDATES: &[&str] = &[
    "ggml-small.en.bin",
    "ggml-small-en.bin",
    "ggml-base.en.bin",
    "ggml-base-en.bin",
    "ggml-medium.en.bin",
    "ggml-medium-en.bin",
];

pub fn default_cli_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/whisper.cpp/build/bin/Release/whisper-cli.exe")
}

pub fn resolve_cli_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AQUA_WHISPER_CLI_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    // An installed copy has no `vendor/`: look beside the program first.
    if let Some(p) = crate::engines::installed_engine("whisper/whisper-cli.exe") {
        return Some(p);
    }
    let candidates = [
        default_cli_binary(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/whisper.cpp/build/bin/whisper-cli.exe"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/whisper.cpp/build/bin/whisper-cli"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/whisper.cpp/build/bin/Release/whisper-cli"),
    ];
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

pub fn resolve_model_file(model_dir: &Path) -> Option<PathBuf> {
    for name in MODEL_CANDIDATES {
        let p = model_dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn model_bundle_valid(model_dir: &Path) -> bool {
    resolve_model_file(model_dir).is_some()
}

/// Whisper is shown the assistant's name first, so it writes "Aqua" rather than "ACWA" or "aquah".
const WAKE_WORD_PROMPT: &str = "Hey Aqua.";

static LISTENING_CHECKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How a transcription is run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// A person's own request: the answer waits for it, so it may use most of the machine for a moment.
    Request,
    /// The listeners that run all day (the wake word, the spoken "stop"): a few threads at a low priority, so that they never
    /// slow down the model that is answering.
    Listening,
}

impl Lane {
    /// Threads for `whisper-cli` (`AQUA_WHISPER_THREADS` for requests, `AQUA_WHISPER_LISTEN_THREADS` for the listeners).
    /// Measured on this laptop (4 cores, 8 threads) with Aqua's settings: the small model on 4 s of speech took 2.2 s with 2
    /// threads, 1.8 s with 4, 1.7 s with 6 and 1.7 s with 8; the base model on 1.5 s of speech 0.71 s with 2 threads and 0.49 s
    /// with 4 or more. So a request gets two threads fewer than the machine has (the window and the microphone need some), and a
    /// listener, which runs all day, gets two: a fifth slower, and it takes a third of the CPU time.
    pub fn threads(self) -> usize {
        let (var, default) = match self {
            Lane::Request => ("AQUA_WHISPER_THREADS", crate::cpu::logical_processors().saturating_sub(2).max(2)),
            Lane::Listening => ("AQUA_WHISPER_LISTEN_THREADS", 2),
        };
        std::env::var(var).ok().and_then(|s| s.parse::<usize>().ok()).filter(|n| (1..=64).contains(n)).unwrap_or(default)
    }

    /// How long one run may take before it is stopped. A check of the wake word or of "stop" takes a second or two; the web server gives up on
    /// one after 12 s, but the program went on to the full 90 s, and the single place for such checks stayed taken until it ended, so a
    /// speech program that hung left the orb deaf to its wake word for a minute and a half. A request keeps the long limit.
    pub fn timeout(self) -> std::time::Duration {
        match self {
            Lane::Request => WHISPER_TIMEOUT,
            Lane::Listening => std::time::Duration::from_secs(20),
        }
    }
}

/// Run the program below the normal priority: whatever else is running (the model that is answering, the window) goes first.
#[cfg(windows)]
fn lower_priority(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    cmd.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
}

#[cfg(not(windows))]
fn lower_priority(_cmd: &mut Command) {}

/// Transcribe a WAV/FLAC/MP3 file using upstream `whisper-cli`.
pub fn transcribe_file(cli: &Path, model: &Path, audio_file: &Path) -> Result<String> {
    transcribe_file_in(cli, model, audio_file, Lane::Request)
}

/// The same for a given [`Lane`].
pub fn transcribe_file_in(cli: &Path, model: &Path, audio_file: &Path, lane: Lane) -> Result<String> {
    transcribe_file_prompted(cli, model, audio_file, lane, WAKE_WORD_PROMPT)
}

/// The same with the text Whisper is primed with given: the assistant's name by default, a person's own words (their terms: names, products, jargon) when dictating.
pub fn transcribe_file_prompted(cli: &Path, model: &Path, audio_file: &Path, lane: Lane, prompt: &str) -> Result<String> {
    let context = audio_context_for(wav_seconds(audio_file));
    let what = format!(
        "Running whisper-cli (model {:?}, {} bytes of audio{})",
        model.file_name().unwrap_or_default(),
        std::fs::metadata(audio_file).map(|m| m.len()).unwrap_or(0),
        context.map(|c| format!(", audio context {c}")).unwrap_or_default()
    );
    match lane {
        Lane::Request => info!("{what}"),
        // The wake and stop listeners run all day (about 1,200 times a day, 37 % of the whole log): one line in a hundred, as a count.
        // `RUST_LOG=aqua_audio_adapter::whisper_cli=debug` shows every one.
        Lane::Listening => {
            tracing::debug!("{what}");
            let checks = LISTENING_CHECKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if checks % 100 == 0 {
                info!("wake and stop listening: {checks} checks since the core started");
            }
        }
    }
    let mut cmd = Command::new(cli);
    cmd.arg("-m").arg(model).arg("-f").arg(audio_file);
    cmd.args(decoding_args(lane, prompt));
    if lane == Lane::Listening {
        lower_priority(&mut cmd);
    }
    if let Some(c) = context {
        cmd.arg("-ac").arg(c.to_string());
    }
    // Only what the voice-activity model calls speech is transcribed.
    if let Some(vad) = vad_model_for(model) {
        cmd.arg("--vad").arg("-vm").arg(vad).arg("-vspd").arg("150");
    }
    let output = crate::process::run_with_deadline(cmd, None, lane.timeout()).context("whisper-cli")?;
    if !output.status.success() {
        anyhow::bail!("whisper-cli failed: {}", String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("no message"));
    }
    Ok(normalize_transcript(&String::from_utf8_lossy(&output.stdout)))
}

/// Length of a plain PCM WAV file in seconds, read from its header. None for anything else (the caller then lets Whisper use its
/// full window).
pub fn wav_seconds(path: &Path) -> Option<f32> {
    use std::io::Read;
    let mut head = Vec::with_capacity(4096);
    let len = std::fs::File::open(path).ok()?.take(4096).read_to_end(&mut head).ok()?;
    wav_seconds_from_header(&head, std::fs::metadata(path).ok()?.len(), len)
}

fn wav_seconds_from_header(head: &[u8], file_len: u64, _read: usize) -> Option<f32> {
    if head.len() < 12 || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut byte_rate = 0u32;
    while pos + 8 <= head.len() {
        let id = &head[pos..pos + 4];
        let size = u32::from_le_bytes([head[pos + 4], head[pos + 5], head[pos + 6], head[pos + 7]]);
        if id == b"fmt " && pos + 20 <= head.len() {
            byte_rate = u32::from_le_bytes([head[pos + 16], head[pos + 17], head[pos + 18], head[pos + 19]]);
        } else if id == b"data" {
            let data_start = (pos + 8) as u64;
            // A streamed file can say 0 or 0xFFFFFFFF here: the rest of the file is the audio.
            let bytes = if size == 0 || size == u32::MAX || data_start + u64::from(size) > file_len { file_len.saturating_sub(data_start) } else { u64::from(size) };
            return (byte_rate > 0).then(|| bytes as f32 / byte_rate as f32);
        }
        pos += 8 + size as usize + (size as usize & 1);
    }
    None
}

/// How much of Whisper's 30-second window to use for a clip of this length (`-ac`, in units of 20 ms). Whisper always encodes
/// a whole 30-second window, so a one-second command costs as much as a long dictation: on this machine 4.4 to 5.1 s for clips of 1 to
/// 3 seconds with the small model, and 1.5 to 2.5 s with the context cut to what the clip needs, with the same text.
/// The window is the clip plus two seconds of room, never less than 7.7 s, and the full window for anything over 12 seconds, so a
/// longer sentence is never cut off. Set `AQUA_WHISPER_FULL_CONTEXT=1` to turn this off.
pub fn audio_context_for(seconds: Option<f32>) -> Option<u32> {
    if std::env::var_os("AQUA_WHISPER_FULL_CONTEXT").is_some() {
        return None;
    }
    let s = seconds?;
    if !(0.0..=12.0).contains(&s) {
        return None;
    }
    let units = ((s + 2.0) * 50.0).ceil() as u32;
    Some((units.div_ceil(32) * 32).clamp(384, 1500))
}

/// The voice-activity model (Silero, in whisper.cpp's format: `ggml-silero-*.bin`) kept beside the Whisper models, if there is one.
/// With it Whisper transcribes only the stretches the model calls speech. Without it Whisper hears wind, typing or a fan as
/// words and invents them ("Hello.", "Hello everyone.", "Thank you."), which Aqua then answers. Measured on loud wind, typing
/// and fan clips with the small and base models: every one came back as an invented phrase without it, and empty (and three to
/// ten times faster) with it, while speech under the same noise and real speech were transcribed as before.
/// `scripts/fetch_whisper_vad.ps1` downloads it; `AQUA_WHISPER_VAD=off` turns it off.
pub fn vad_model_for(whisper_model: &Path) -> Option<PathBuf> {
    if std::env::var("AQUA_WHISPER_VAD").is_ok_and(|v| v.eq_ignore_ascii_case("off") || v == "0") {
        return None;
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(whisper_model.parent()?)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("ggml-silero") && n.ends_with(".bin")))
        .collect();
    found.sort();
    found.pop()
}

/// How long one whisper-cli run may take before it is stopped.
const WHISPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// The decoding options every run uses: English, no timestamps or progress, non-speech tokens suppressed, primed
/// with the assistant's name, and greedy (`-bs 1`). The default is a beam search of 5, which on some clips ran away: with the
/// shortened audio window and no timestamps, one of 36 cut points of the same recording took 209 decode steps and 4.5 s instead
/// of 0.7 s (7 times as long), and a long wrong transcript could reach the router as the person's request. Greedy decoding
/// stopped it (3 and 8 steps) and is also a little faster.
fn decoding_args(lane: Lane, prompt: &str) -> Vec<String> {
    ["-l", "en", "-t", &lane.threads().to_string(), "-bs", "1", "-nt", "-np", "-sns", "--prompt", prompt].iter().map(|s| s.to_string()).collect()
}

/// The words inside the markers Whisper writes for what is not speech: `[BLANK_AUDIO]`, `(mumbling)`, `[MUSIC]`, `(background noise)`.
const NOISE_WORDS: &[&str] = &[
    "blank", "audio", "silence", "silent", "music", "mumbling", "noise", "background", "laughter", "laughing", "applause", "inaudible", "static", "cough", "coughing", "sigh", "sighs", "sighing",
    "breathing", "wind", "typing", "speaking", "foreign", "language", "crowd", "chatter", "clicking", "beep", "sound", "sounds", "no", "speech", "unintelligible", "indistinct", "whispering",
];

/// Is this what is between brackets one of Whisper's markers for noise (and not words a person said, like "[TODO]" or "(see page 3)")?
fn is_noise_marker(inner: &str) -> bool {
    let words: Vec<String> = inner.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect();
    !words.is_empty() && words.len() <= 4 && words.iter().all(|w| NOISE_WORDS.contains(&w.as_str()))
}

/// Whisper writes things like `[BLANK_AUDIO]`, `(mumbling)` or `♪` for silence and noise, and invents short
/// sign-offs ("Thank you.", "Bye.") for it. None of that is something a person said, so it becomes an empty
/// transcript and nothing downstream answers it. Real words around such a marker are kept, and so are words in brackets that are
/// not a marker ("[TODO] fix this", "(see page 3)"): what is dictated into a document is not Whisper's to remove.
pub fn normalize_transcript(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut kept = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let closer = match c {
            '[' => Some(']'),
            '(' => Some(')'),
            '{' => Some('}'),
            _ => None,
        };
        if let Some(close) = closer {
            // The marker ends at the first closing bracket of its kind, or at the end of the text when there is none (a marker cut short).
            let end = chars[i + 1..].iter().position(|x| *x == close).map(|p| i + 1 + p);
            let inner: String = chars[i + 1..end.unwrap_or(chars.len())].iter().collect();
            if is_noise_marker(&inner) {
                i = end.map_or(chars.len(), |e| e + 1);
                continue;
            }
        }
        if !matches!(c, '\u{266A}' | '\u{266B}' | '\u{1F3B5}') {
            kept.push(c);
        }
        i += 1;
    }
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bare_words(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect::<String>().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Short sign-offs and noises that Whisper invents for silence or room noise ("Thank you.", "Bye.", "Um...").
/// They are also things a person really says, so whether to drop one is decided by the caller, who knows the audio
/// level and whether a conversation is in progress.
pub fn is_filler(text: &str) -> bool {
    const FILLER: &[&str] = &["", "you", "thank you", "thanks", "thanks for watching", "thank you for watching", "bye", "bye bye", "okay", "uh", "um", "hmm", "mm", "mhm", "mmhmm", "uhhuh"];
    FILLER.contains(&bare_words(text).as_str())
}

/// Sentences addressed to an audience, which Whisper learned from video soundtracks and invents for noise or for a television in
/// the room ("Hello everyone.", "Thanks for watching.", "Please subscribe."). Nobody says them to an assistant, so unlike
/// [`is_filler`] they are dropped at any volume, in a conversation too. Said twice in a row they still count.
pub fn is_audience_phrase(text: &str) -> bool {
    const AUDIENCE: &[&str] = &[
        "hello everyone", "hi everyone", "hey everyone", "hello everybody", "hi everybody", "hello guys", "hi guys", "hey guys", "hello friends",
        "welcome back", "welcome to my channel", "welcome to the channel", "welcome to this video", "thanks for watching", "thank you for watching",
        "thank you so much for watching", "please subscribe", "subscribe to my channel", "like and subscribe", "see you next time",
        "see you in the next video", "see you in the next one", "bye everyone", "goodbye everyone",
    ];
    // Commas separate too: "Hello everyone, welcome back."
    let pieces = pieces_of(text, true);
    !pieces.is_empty() && pieces.iter().all(|s| AUDIENCE.contains(&s.as_str()) || is_filler(s)) && pieces.iter().any(|s| AUDIENCE.contains(&s.as_str()))
}

/// The sentences of a transcript (and the parts between commas when `commas`), each as bare lower-case words with no
/// punctuation, empty ones left out.
fn pieces_of(text: &str, commas: bool) -> Vec<String> {
    let separators: &[char] = if commas { &['.', '?', '!', ',', ';', '\u{2026}', '\n'] } else { &['.', '?', '!', '\u{2026}', '\n'] };
    text.split(separators).map(bare_words).filter(|s| !s.is_empty()).collect()
}

/// A sentence of four words or more that comes twice in one transcript is Whisper looping ("It's the reason why I'm here today. It's
/// the reason why I'm here today."): a person does not say the same long sentence twice in one breath. Short ones are left alone
/// ("Stop. Stop." and "Hey Aqua. Hey Aqua." are people repeating themselves).
pub fn repeats_a_sentence(text: &str) -> bool {
    let sentences = pieces_of(text, false);
    sentences.iter().enumerate().any(|(i, s)| s.split_whitespace().count() >= 4 && sentences[..i].contains(s))
}

/// The transcript is only the priming text Whisper was given (it sometimes repeats it on near-silence).
pub fn is_prompt_echo(text: &str) -> bool {
    bare_words(text) == bare_words(WAKE_WORD_PROMPT)
}

/// Removes a file when it goes out of scope, however the function that made it ends.
struct TempFile(std::path::PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Transcribe WAV bytes held in memory. They are written to a short-lived temp file (removed on every path, errors
/// and panics included) and transcribed like any file.
///
/// Streaming them to whisper-cli's stdin (`-f -`) does not work with this build: it reads the audio and then prints
/// nothing and exits successfully, so the answer was always empty.
pub fn transcribe_wav_bytes(cli: &Path, model: &Path, wav_bytes: &[u8]) -> Result<String> {
    transcribe_wav_bytes_in(cli, model, wav_bytes, Lane::Request)
}

/// The same for a given [`Lane`].
pub fn transcribe_wav_bytes_in(cli: &Path, model: &Path, wav_bytes: &[u8], lane: Lane) -> Result<String> {
    transcribe_wav_bytes_prompted(cli, model, wav_bytes, lane, WAKE_WORD_PROMPT)
}

/// The same with the text Whisper is primed with given (an empty one is the assistant's name).
pub fn transcribe_wav_bytes_prompted(cli: &Path, model: &Path, wav_bytes: &[u8], lane: Lane, prompt: &str) -> Result<String> {
    let prompt = if prompt.trim().is_empty() { WAKE_WORD_PROMPT } else { prompt };
    if wav_bytes.is_empty() {
        return Ok(String::new());
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let temp = TempFile(std::env::temp_dir().join(format!("aqua_whisper_mem_{}_{stamp}.wav", std::process::id())));
    std::fs::write(&temp.0, wav_bytes)?;
    transcribe_file_prompted(cli, model, &temp.0, lane, prompt)
}

pub fn cli_available() -> bool {
    resolve_cli_binary().is_some()
}

pub fn warn_if_cli_missing() {
    if !cli_available() {
        warn!(
            "whisper-cli not built. Run scripts/build_whisper_cli.ps1 (see data/models/whisper/README.md)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_transcript as n;

    #[test]
    fn markers_for_silence_and_noise_are_not_speech() {
        for raw in ["[BLANK_AUDIO]", " [BLANK_AUDIO]\n", "(mumbling)", "(silence)", "[MUSIC]", "\u{266A}", "[BLANK_AUDIO] (mumbling)", "", "   "] {
            assert_eq!(n(raw), "", "{raw:?}");
        }
    }

    #[test]
    fn words_in_brackets_that_a_person_said_are_kept_and_only_the_markers_for_noise_go() {
        assert_eq!(n("[TODO] fix this"), "[TODO] fix this");
        assert_eq!(n("see the chart (see page 3) first"), "see the chart (see page 3) first");
        assert_eq!(n("call me {maybe} tomorrow"), "call me {maybe} tomorrow");
        assert_eq!(n("hello [MUSIC] world"), "hello world");
        assert_eq!(n("hello (background noise) world"), "hello world");
        assert_eq!(n("[BLANK_AUDIO"), "", "a marker cut short");
        assert_eq!(n("it is [inaudible] and (laughs) fine"), "it is and (laughs) fine");
    }

    #[test]
    fn the_sign_offs_whisper_invents_are_recognised_as_fillers_but_not_removed_here() {
        use super::is_filler;
        for raw in ["Thank you.", "thank you", "Thanks for watching!", "Bye.", "you", "Um...", "Okay.", "Mm-hmm"] {
            assert!(is_filler(raw), "{raw:?}");
            assert_eq!(n(raw), raw.trim(), "normalising keeps what a person may really have said: {raw:?}");
        }
        for raw in ["Stop.", "Yes.", "Thank you, Aqua", "Okay, open the calendar"] {
            assert!(!is_filler(raw), "{raw:?}");
        }
    }

    #[test]
    fn the_priming_text_repeated_back_is_recognised() {
        use super::is_prompt_echo;
        assert!(is_prompt_echo("Hey Aqua.") && is_prompt_echo("  hey, AQUA ") && !is_prompt_echo("Hey Aqua, what time is it"));
    }

    #[test]
    fn real_words_survive_even_with_a_marker_around_them() {
        assert_eq!(n("Hey Aqua, what time is it?"), "Hey Aqua, what time is it?");
        assert_eq!(n("[BLANK_AUDIO] open the calendar (cough) please"), "open the calendar please");
        assert_eq!(n("Thank you for the reminder, Aqua"), "Thank you for the reminder, Aqua");
        assert_eq!(n("  turn   it\n up "), "turn it up");
    }
}

#[cfg(test)]
mod lane_tests {
    use super::*;

    #[test]
    fn a_dictation_clip_is_primed_with_the_persons_own_words_and_the_rest_with_the_assistants_name() {
        let own = decoding_args(Lane::Request, "Kubernetes, Aqua, Kokoro");
        let at = own.iter().position(|a| a == "--prompt").expect("a prompt is set");
        assert_eq!(own[at + 1], "Kubernetes, Aqua, Kokoro");
        assert!(!own.contains(&WAKE_WORD_PROMPT.to_string()), "the person's terms replace the name, they are not added to it");
    }

    #[test]
    fn every_run_decodes_greedily_and_the_listeners_use_few_threads() {
        let request = decoding_args(Lane::Request, WAKE_WORD_PROMPT);
        let listening = decoding_args(Lane::Listening, WAKE_WORD_PROMPT);
        for args in [&request, &listening] {
            let at = args.iter().position(|a| a == "-bs").expect("a beam size is set");
            assert_eq!(args[at + 1], "1", "greedy decoding: the default beam search of 5 ran away on some clips");
            assert!(args.contains(&"-nt".to_string()) && args.contains(&"-sns".to_string()) && args.contains(&WAKE_WORD_PROMPT.to_string()));
        }
        let threads = |args: &[String]| args[args.iter().position(|a| a == "-t").unwrap() + 1].parse::<usize>().unwrap();
        assert_eq!(threads(&listening), 2, "a listener that runs all day takes two threads");
        let logical = crate::cpu::logical_processors();
        assert_eq!(threads(&request), logical.saturating_sub(2).max(2), "a request leaves two logical processors free");
        assert!(threads(&request) >= threads(&listening));
    }

    /// Found in a review: a check of the wake word that hung kept its single place taken for the full 90 s, long after the web server had
    /// given up on it (12 s), and the orb could not hear its wake word meanwhile.
    #[test]
    fn a_hung_listening_check_is_stopped_soon_after_the_server_gave_up_and_a_request_keeps_the_long_limit() {
        assert!(Lane::Listening.timeout() > std::time::Duration::from_secs(12), "longer than the web server waits for it");
        assert!(Lane::Listening.timeout() <= std::time::Duration::from_secs(30));
        assert_eq!(Lane::Request.timeout(), WHISPER_TIMEOUT);
        assert!(Lane::Request.timeout() >= std::time::Duration::from_secs(60));
    }
}

#[cfg(test)]
mod vad_tests {
    use super::*;

    #[test]
    fn the_voice_activity_model_is_found_beside_the_whisper_model_and_the_newest_one_wins() {
        let dir = std::env::temp_dir().join(format!("aqua-vad-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("ggml-small.en.bin");
        std::fs::write(&model, b"x").unwrap();
        assert!(vad_model_for(&model).is_none(), "nothing there yet");
        std::fs::write(dir.join("ggml-silero-v5.1.2.bin"), b"x").unwrap();
        std::fs::write(dir.join("ggml-silero-v6.2.0.bin"), b"x").unwrap();
        std::fs::write(dir.join("ggml-base.en.bin"), b"x").unwrap();
        assert_eq!(vad_model_for(&model).unwrap().file_name().unwrap(), "ggml-silero-v6.2.0.bin");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    fn wav(seconds: f32, extra_chunk: bool) -> Vec<u8> {
        let rate = 16000u32;
        let n = (seconds * rate as f32) as u32 * 2;
        let mut v = b"RIFF".to_vec();
        v.extend((36 + n).to_le_bytes());
        v.extend(b"WAVE");
        if extra_chunk {
            v.extend(b"LIST");
            v.extend(5u32.to_le_bytes());
            v.extend([1, 2, 3, 4, 5, 0]); // padded to an even length
        }
        v.extend(b"fmt ");
        v.extend(16u32.to_le_bytes());
        v.extend([1, 0, 1, 0]);
        v.extend(rate.to_le_bytes());
        v.extend((rate * 2).to_le_bytes());
        v.extend([2, 0, 16, 0]);
        v.extend(b"data");
        v.extend(n.to_le_bytes());
        v.extend(vec![0u8; n as usize]);
        v
    }

    #[test]
    fn the_length_of_a_wav_is_read_from_its_header() {
        for (secs, extra) in [(1.6f32, false), (5.0, true), (0.3, false)] {
            let b = wav(secs, extra);
            let got = wav_seconds_from_header(&b[..b.len().min(4096)], b.len() as u64, 0).unwrap();
            assert!((got - secs).abs() < 0.01, "{secs}: {got}");
        }
        assert!(wav_seconds_from_header(b"not a wav file at all", 21, 0).is_none());
        assert!(wav_seconds_from_header(&[], 0, 0).is_none());
    }

    #[test]
    fn a_streamed_wav_with_an_unknown_size_uses_the_rest_of_the_file() {
        let mut b = wav(2.0, false);
        let at = b.windows(4).position(|w| w == b"data").unwrap() + 4;
        b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let got = wav_seconds_from_header(&b[..4096], b.len() as u64, 0).unwrap();
        assert!((got - 2.0).abs() < 0.01, "{got}");
    }

    #[test]
    fn short_clips_use_a_short_window_and_long_ones_the_whole_thing() {
        assert_eq!(audio_context_for(Some(1.1)), Some(384));
        assert_eq!(audio_context_for(Some(2.7)), Some(384), "never less than 7.7 seconds");
        assert_eq!(audio_context_for(Some(6.0)), Some(416));
        assert_eq!(audio_context_for(Some(12.0)), Some(704));
        assert!(audio_context_for(Some(12.5)).is_none(), "a long sentence gets the full window");
        assert!(audio_context_for(None).is_none(), "a file whose length is unknown too");
        // The window always covers the clip with two seconds to spare.
        for tenth in 1..=120 {
            let secs = tenth as f32 / 10.0;
            let ctx = audio_context_for(Some(secs)).unwrap();
            assert!(ctx as f32 * 0.02 >= secs + 1.9, "{secs}: {ctx}");
        }
    }
}

#[cfg(test)]
mod temp_tests {
    use super::TempFile;

    #[test]
    fn the_temporary_audio_file_is_removed_when_the_guard_goes_away_even_on_a_panic() {
        let path = std::env::temp_dir().join(format!("aqua_tempfile_test_{}.wav", std::process::id()));
        std::fs::write(&path, b"voice").unwrap();
        {
            let _guard = TempFile(path.clone());
            assert!(path.exists());
        }
        assert!(!path.exists(), "removed on a normal exit");
        std::fs::write(&path, b"voice").unwrap();
        let p2 = path.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = TempFile(p2);
            panic!("transcription blew up");
        });
        assert!(!path.exists(), "removed when the work panicked");
    }
}

#[cfg(test)]
mod room_noise_tests {
    use super::*;

    #[test]
    fn sentences_for_an_audience_are_recognised_alone_or_repeated() {
        for t in ["Hello everyone.", "Hello everyone. Hello everyone.", "Hello everyone, welcome back.", "Thanks for watching! Please subscribe.", "Thank you for watching.", "Hi everyone... Hello guys"] {
            assert!(is_audience_phrase(t), "{t}");
        }
        for t in ["Hello Aqua", "Hello, open Notepad", "Say hello everyone to my friends", "Thank you.", "Welcome back to the desktop, open Chrome", "", "Hello everyone open Notepad", "See you soon"] {
            assert!(!is_audience_phrase(t), "{t}");
        }
    }

    #[test]
    fn a_long_sentence_said_twice_is_a_loop_and_short_repeats_are_people() {
        assert!(repeats_a_sentence("Hi. It's the reason why I'm here today. It's the reason why I'm here today. The"));
        assert!(repeats_a_sentence("I will see you tomorrow! I will see you tomorrow!"));
        for t in ["Stop. Stop.", "Hey Aqua. Hey Aqua.", "Open Notepad. Open Notepad.", "Yes yes yes yes", "It's the reason why I'm here today.", "Open the file. Then close the file.", ""] {
            assert!(!repeats_a_sentence(t), "{t}");
        }
    }
}
