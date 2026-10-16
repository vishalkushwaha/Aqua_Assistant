//! Dictation: what the core does for "hold a key, speak, release, and clean text appears in the app you are in".
//!
//! The key hook and the insertion into the other app belong to the desktop shell (they need the person's own desktop session); the core owns what
//! has no such need and can be tested without one: the person's settings (`data/dictation.json`: cleanup level, own words, replacements, snippets,
//! limits), the cleanup of the transcript by rules (see [`cleanup`]), and the routes the shell calls (`GET`/`PUT /api/v1/dictation/settings`,
//! `POST /api/v1/dictation/clean`, `POST /api/v1/dictation/transcribe`).
//!
//! Dictation is the person's own keyboard, not the agent's: nothing here runs an agent, uses a tool or presses a key, and the voice caller of the
//! agent still cannot type through it. Audio is not kept.

pub mod cleanup;
pub mod command;
pub mod model_tier;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MAX_TERMS: usize = 200;
pub const MAX_REPLACEMENTS: usize = 200;
pub const MAX_SNIPPETS: usize = 100;
/// A word of the person's or a replacement is a short thing; a snippet may be a few lines.
pub const MAX_FIELD_CHARS: usize = 60;
pub const MAX_SNIPPET_CHARS: usize = 500;
/// The longest one dictation may last (the card's cap), and what the settings may ask for.
pub const MAX_MINUTES: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// The transcript as it came, trimmed.
    None,
    /// By rules: fillers, stutters, spoken corrections, own replacements, spacing and capitals, snippets.
    #[default]
    Light,
    /// Rewriting by a small local model, **S1-mini by Superwhisper** (see [`model_tier`]): opt-in (`AQUA_DICTATION_MEDIUM=on`), only with the model file on this machine and a
    /// recorded pass of the golden corpus for that file. When any of those is missing it is read as `light` and the answer says which.
    Medium,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    pub trigger: String,
    pub text: String,
}

/// A way of writing for one app (`Slack`: "casual, short sentences"), kept as a plain prompt for command mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Style {
    pub app: String,
    pub prompt: String,
}

pub const MAX_STYLES: usize = 20;
pub const MAX_STYLE_PROMPT_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub cleanup: Level,
    /// Words and names that are hard to hear (they are put in Whisper's prompt when the engine can take one).
    pub terms: Vec<String>,
    pub replacements: Vec<Replacement>,
    pub snippets: Vec<Snippet>,
    /// Ways of writing for command mode (see [`command`]).
    pub styles: Vec<Style>,
    /// One dictation lasts at most this many minutes.
    pub max_minutes: u32,
    /// `en` (the model on this machine is English only).
    pub language: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { cleanup: Level::Light, terms: Vec::new(), replacements: Vec::new(), snippets: Vec::new(), styles: Vec::new(), max_minutes: MAX_MINUTES, language: "en".into() }
    }
}

impl Settings {
    /// What is wrong with these settings, in plain words, or `Ok`.
    pub fn validate(&self) -> Result<(), String> {
        if self.terms.len() > MAX_TERMS || self.replacements.len() > MAX_REPLACEMENTS || self.snippets.len() > MAX_SNIPPETS {
            return Err(format!("at most {MAX_TERMS} words, {MAX_REPLACEMENTS} replacements and {MAX_SNIPPETS} snippets"));
        }
        let field = |what: &str, s: &str, max: usize| -> Result<(), String> {
            if s.trim().is_empty() || s.chars().count() > max {
                return Err(format!("{what} must be 1 to {max} characters"));
            }
            if s.chars().any(|c| c.is_control() && c != '\n') {
                return Err(format!("{what} must not hold control characters"));
            }
            Ok(())
        };
        for t in &self.terms {
            field("a word", t, MAX_FIELD_CHARS)?;
        }
        for r in &self.replacements {
            field("a replacement's \"from\"", &r.from, MAX_FIELD_CHARS)?;
            field("a replacement's \"to\"", &r.to, MAX_FIELD_CHARS)?;
        }
        for s in &self.snippets {
            field("a snippet's trigger", &s.trigger, MAX_FIELD_CHARS)?;
            field("a snippet's text", &s.text, MAX_SNIPPET_CHARS)?;
        }
        if self.styles.len() > MAX_STYLES {
            return Err(format!("at most {MAX_STYLES} styles"));
        }
        for st in &self.styles {
            field("a style's app", &st.app, MAX_FIELD_CHARS)?;
            field("a style's prompt", &st.prompt, MAX_STYLE_PROMPT_CHARS)?;
        }
        if !(1..=MAX_MINUTES).contains(&self.max_minutes) {
            return Err(format!("max_minutes is 1 to {MAX_MINUTES}"));
        }
        if self.language != "en" {
            return Err("language is \"en\": the speech model on this machine is English only".into());
        }
        Ok(())
    }

    /// The words for Whisper's prompt: the person's own terms, comma separated, short enough for the model's prompt window. A dictation clip is primed with them.
    pub fn initial_prompt(&self) -> String {
        let mut out = String::new();
        for t in &self.terms {
            let t = t.trim();
            if out.len() + t.len() + 2 > 800 {
                break;
            }
            if !out.is_empty() {
                out.push_str(", ");
            }
            out.push_str(t);
        }
        out
    }
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("dictation.json")
}

/// Reads `data/dictation.json`. A missing file is the defaults; a file that cannot be read is an error that names the problem, and dictation then
/// uses the defaults (a wrong file never stops the person from dictating).
pub fn load(data_dir: &Path) -> Result<Settings, String> {
    let p = path(data_dir);
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    let settings: Settings = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("{}: {e}", p.display()))?;
    settings.validate().map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(settings)
}

/// Writes the settings (checked first), through a temporary file that is flushed and renamed over the old one (`fsutil::write_atomic_durable`), so that a crash cannot leave half of one.
pub fn save(data_dir: &Path, settings: &Settings) -> Result<(), String> {
    settings.validate()?;
    let p = path(data_dir);
    aqua_tool_host::fsutil::write_atomic_durable(&p, &serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// What `POST /api/v1/dictation/clean` and `transcribe` answer with.
#[derive(Debug, Clone, Serialize)]
pub struct Cleaned {
    pub text: String,
    /// What was done to the transcript, for the person to see.
    pub applied: Vec<String>,
    /// Said when the person asked for something that was not done (`medium` without its model, or with a part that the model's text was not used for).
    pub note: Option<String>,
}

/// Cleans a transcript by rules, with the person's settings. The medium level is the model's (see [`clean_for`]); here it is read as `light`, and the answer says so.
pub fn clean_with(text: &str, settings: &Settings) -> Cleaned {
    let out = cleanup::clean(text, settings);
    let note = (settings.cleanup == Level::Medium).then(|| "the medium level needs the model of this machine and was not asked for here: the text was cleaned at the light level".to_string());
    Cleaned { text: out.text, applied: out.applied, note }
}

/// Cleans a transcript at the person's level. At `medium` the model of the tier writes it when it can be used (switched on, the model file there, the golden corpus passed for that
/// file), and when it cannot the text is cleaned at the light level and the note says what is missing. Starts a program when the model is used: call it where blocking is allowed.
pub fn clean_for(text: &str, settings: &Settings, data_dir: &Path) -> Cleaned {
    if settings.cleanup != Level::Medium {
        return clean_with(text, settings);
    }
    match model_tier::available(data_dir) {
        Ok(model) => model_tier::clean_medium(text, settings, &model),
        Err(why) => {
            let mut out = clean_with(text, settings);
            out.note = Some(format!("{why}: the text was cleaned at the light level"));
            out
        }
    }
}

/// Why a clip is not turned into text at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotText {
    /// Longer than the person's limit (`max_minutes`).
    TooLong { limit_minutes: u32 },
    /// Silence, wind, typing or a fan: Whisper invents words for those, so it is not asked.
    NoSpeech,
}

/// A clip (16 kHz mono) against the person's limit and against the speech gate (`not_speech` says whether there is nobody talking in it).
pub fn check_clip(pcm16k: &[f32], settings: &Settings, not_speech: &dyn Fn(&[f32]) -> bool) -> Result<(), NotText> {
    if pcm16k.len() as f32 / 16_000.0 / 60.0 > settings.max_minutes as f32 {
        return Err(NotText::TooLong { limit_minutes: settings.max_minutes });
    }
    if not_speech(pcm16k) {
        return Err(NotText::NoSpeech);
    }
    Ok(())
}

/// What Whisper wrote for a clip that passed the gate, or nothing when it is its own invention: its priming text repeated on a quiet clip, a sign-off for an audience, a long
/// sentence looped. A "Thank you." said at a normal volume is what the person said and stays. `level` is how loud the loudest second of the clip is.
pub fn heard_text(raw: &str, level: f32) -> String {
    let raw = raw.trim();
    if raw.is_empty() || crate::pipeline::is_invented_by_whisper(raw, true, level, "Hey Aqua") {
        return String::new();
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_dictation_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_missing_file_is_the_defaults_and_what_is_saved_is_what_is_read() {
        let d = dir("roundtrip");
        assert_eq!(load(&d).unwrap(), Settings::default());
        let s = Settings {
            cleanup: Level::None,
            terms: vec!["Kubernetes".into(), "Aqua".into()],
            replacements: vec![Replacement { from: "jay son".into(), to: "JSON".into() }],
            snippets: vec![Snippet { trigger: "my email".into(), text: "a@b.c".into() }],
            styles: vec![Style { app: "Slack".into(), prompt: "casual".into() }],
            max_minutes: 5,
            language: "en".into(),
        };
        save(&d, &s).unwrap();
        assert_eq!(load(&d).unwrap(), s);
        assert!(!path(&d).with_extension("json.tmp").exists(), "no temporary file is left");
    }

    #[test]
    fn a_wrong_setting_is_refused_with_the_reason_and_a_wrong_file_is_an_error_not_a_crash() {
        let d = dir("wrong");
        let bad = |f: fn(&mut Settings)| {
            let mut s = Settings::default();
            f(&mut s);
            s.validate().unwrap_err()
        };
        assert!(bad(|s| s.terms = vec![String::new()]).contains("a word must be 1 to 60"));
        assert!(bad(|s| s.replacements = vec![Replacement { from: "x".repeat(61), to: "y".into() }]).contains("from"));
        assert!(bad(|s| s.snippets = vec![Snippet { trigger: "t".into(), text: "x".repeat(501) }]).contains("snippet's text"));
        assert!(bad(|s| s.terms = (0..201).map(|i| format!("w{i}")).collect()).contains("at most 200 words"));
        assert!(bad(|s| s.max_minutes = 21).contains("max_minutes"));
        assert!(bad(|s| s.language = "de".into()).contains("English only"));
        assert!(bad(|s| s.terms = vec!["a\u{7}b".into()]).contains("control"));
        assert!(save(&d, &Settings { max_minutes: 0, ..Settings::default() }).is_err() && !path(&d).exists(), "nothing is written for settings that are wrong");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(path(&d), "{ \"cleanup\": \"loud\" }").unwrap();
        assert!(load(&d).unwrap_err().contains("dictation.json"));
        std::fs::write(path(&d), "{ \"unknown_key\": 1 }").unwrap();
        assert!(load(&d).is_err(), "a typo in a key is an error and not silently dropped");
        std::fs::write(path(&d), "\u{feff}{ \"cleanup\": \"none\" }").unwrap();
        assert_eq!(load(&d).unwrap().cleanup, Level::None, "a file saved with a byte-order mark still reads");
    }

    #[test]
    fn a_clip_with_nobody_talking_or_longer_than_the_limit_is_not_turned_into_text() {
        let strict = |pcm: &[f32]| !aqua_audio_adapter::speech_gate::analyze(pcm, aqua_audio_adapter::speech_gate::Strictness::Strict).is_speech();
        let settings = Settings { max_minutes: 1, ..Settings::default() };
        // Silence, and loud noise that has no voice in it.
        let silence = vec![0.0f32; 16_000 * 3];
        assert_eq!(check_clip(&silence, &settings, &strict), Err(NotText::NoSpeech));
        let mut state: u64 = 0x2545F4914F6CDD1D;
        let noise: Vec<f32> = (0..16_000 * 3)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((state >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.6
            })
            .collect();
        assert_eq!(check_clip(&noise, &settings, &strict), Err(NotText::NoSpeech), "wind, typing and a fan are loud and are not speech");
        // Longer than the limit is refused before anything else is looked at.
        let long = vec![0.0f32; 16_000 * 61];
        assert_eq!(check_clip(&long, &settings, &strict), Err(NotText::TooLong { limit_minutes: 1 }));
        // Where the gate finds a voice, the clip goes on.
        assert_eq!(check_clip(&silence, &settings, &|_| false), Ok(()));
    }

    #[test]
    fn what_whisper_invents_for_a_clip_is_dropped_and_what_was_said_stays() {
        // At a normal volume, a sign-off is what the person said.
        assert_eq!(heard_text("Thank you.", 0.05), "Thank you.");
        assert_eq!(heard_text("  Please send it tomorrow.  ", 0.05), "Please send it tomorrow.");
        // On a quiet clip it is Whisper's, and so is its priming text.
        assert_eq!(heard_text("Thank you.", 0.002), "");
        assert_eq!(heard_text("Hey Aqua.", 0.002), "");
        // Sentences for an audience, and a long sentence said twice in a breath, are never the person's.
        assert_eq!(heard_text("Thanks for watching.", 0.05), "");
        assert_eq!(heard_text("It is the reason why I am here today. It is the reason why I am here today.", 0.05), "");
        assert_eq!(heard_text("", 0.05), "");
    }

    #[test]
    fn the_own_words_become_a_short_prompt_for_the_speech_model() {
        let s = Settings { terms: vec!["Kubernetes".into(), " Aqua ".into(), "Kokoro".into()], ..Settings::default() };
        assert_eq!(s.initial_prompt(), "Kubernetes, Aqua, Kokoro");
        let many = Settings { terms: (0..200).map(|i| format!("termnumber{i:03}")).collect(), ..Settings::default() };
        assert!(many.initial_prompt().len() <= 800);
        assert_eq!(Settings::default().initial_prompt(), "");
    }

    #[test]
    fn the_rewriting_level_without_its_model_cleans_at_the_light_level_and_says_what_is_missing() {
        let s = Settings { cleanup: Level::Medium, ..Settings::default() };
        let out = clean_with("um hello there", &s);
        assert_eq!(out.text, "Hello there");
        assert!(out.note.unwrap().contains("medium level"));
        assert!(clean_with("um hello", &Settings::default()).note.is_none());
        // With the tier asked for and nothing set up, the person is told which piece is missing and the text is the light level's. (No environment is changed: the tier is off unless
        // AQUA_DICTATION_MEDIUM is on, and this directory has no model or record, so either way the answer is the light level's.)
        let nowhere = std::env::temp_dir().join(format!("aqua_dictation_nowhere_{}", std::process::id()));
        let asked = clean_for("um hello there", &s, &nowhere);
        assert_eq!(asked.text, "Hello there");
        assert!(asked.note.unwrap().contains("light level"));
        assert!(clean_for("um hello", &Settings::default(), &nowhere).note.is_none(), "the light level asks for nothing");
    }
}
