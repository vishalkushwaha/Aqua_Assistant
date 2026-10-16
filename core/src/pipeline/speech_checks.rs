//! Is there speech in the audio: what Whisper makes up out of nothing, how loud a recording is, whether it is a person or a fan, and the copy of a recording without its long silences (move only: this was part of `pipeline.rs`).

use super::*;

/// A filler ("Thank you.", "Okay.") is dropped only when it is probably Whisper's invention: nobody is in a
/// conversation with Aqua, or the audio was too quiet to hold a spoken word. Said at normal volume inside a
/// session it is a real answer ("Okay" to "shall I go ahead?"). Whisper repeating its own priming text on a
/// quiet recording is dropped too.
pub(crate) fn is_invented_by_whisper(text: &str, in_session: bool, level: f32, wakeup_word: &str) -> bool {
    use aqua_audio_adapter::whisper_cli::{is_audience_phrase, is_filler, is_prompt_echo, repeats_a_sentence};
    let quiet = level < QUIET_RMS;
    // Something said to Aqua by name was meant, whatever else is in it (a person repeating "Hey Aqua, open Notepad" twice).
    let addressed = crate::voice_commands::without_wake_phrase(text, wakeup_word).trim().len() < text.trim().len();
    (is_filler(text) && (!in_session || quiet))
        || (is_prompt_echo(text) && quiet)
        // Sentences for an audience ("Hello everyone.") and a long sentence said twice are the television, or Whisper looping,
        // at any volume. Both were routed to the tool agent, which then held the model for minutes.
        || (!addressed && (is_audience_phrase(text) || repeats_a_sentence(text)))
}

/// Below this (about -40 dBFS) a recording is too quiet to hold clear speech.
pub(super) const QUIET_RMS: f32 = 0.01;

/// How loud the loudest second of a recording is. A short "Hey Aqua." inside a long, mostly silent recording averages out to
/// almost nothing over the whole file, and was then judged "too quiet to be speech" (so it was dropped as Whisper's own
/// echo of its priming text); the loudest second is what tells a quiet recording from a short one.
pub(crate) fn loudest_second_rms(samples: &[f32]) -> f32 {
    const WINDOW: usize = 16_000;
    if samples.len() <= WINDOW {
        return rms_of(samples);
    }
    samples.windows(WINDOW).step_by(WINDOW / 2).map(rms_of).fold(0.0f32, f32::max)
}

pub(super) fn rms_of(samples: &[f32]) -> f32 {
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

/// What a turn says when the audio held no speech (silence, noise, or only sounds like a cough).
/// Below this level (about -60 dBFS) audio is digital silence.
pub(super) const SILENCE_RMS: f32 = 0.001;

pub(super) const NO_SPEECH: &str = "IGNORED: no speech detected.";

/// The clip was noise (wind, typing, a fan): nothing was transcribed, nothing is said back, and the orb does not say "Didn't catch that".
pub const NOT_SPEECH: &str = "IGNORED: not speech.";

/// Is there a person talking in these 16 kHz samples? Wind, a fan and typing are loud and Whisper invents words for them, so
/// loudness is not enough. Lenient when the voice-activity model is behind it (it makes the final call), strict when it is not.
pub(crate) fn is_not_speech(stt: &WhisperSttEngine, pcm16k: &[f32]) -> bool {
    use aqua_audio_adapter::speech_gate::{analyze, Strictness};
    let report = analyze(pcm16k, if stt.vad_active() { Strictness::Lenient } else { Strictness::Strict });
    if !report.is_speech() {
        info!("speech gate: not speech ({:?}; voiced {} ms, longest stretch {} ms, loud {} ms): not transcribed", report.verdict, report.voiced_ms, report.longest_run_ms, report.active_ms);
    }
    !report.is_speech()
}

/// The room kept before and after the speech when a recording is trimmed. A word can start or end with a sound that has no pitch
/// ("s", "f", a breath), which the voice finder does not count, and the speech model reads better with a little silence around it.
const TRIM_MARGIN_MS: u32 = 400;

/// Cutting less than this is not worth a copy of the file: the fixed cost of the noise remover and Whisper is the same.
const TRIM_MIN_SAVED_SAMPLES: usize = 16_000 * 3 / 2;

/// A copy of the recording (16 kHz mono) without the long silence around the speech, in `scratch`; `None` when there is no
/// voice to anchor on, little to cut, or it is switched off (`AQUA_TRIM_SILENCE=off`). The recording itself is not changed.
pub(super) fn trimmed_recording(pcm16k: &[f32], scratch: &Path) -> Option<PathBuf> {
    if std::env::var("AQUA_TRIM_SILENCE").is_ok_and(|v| v.eq_ignore_ascii_case("off") || v == "0") {
        return None;
    }
    let (start, end) = aqua_audio_adapter::speech_gate::speech_span(pcm16k, TRIM_MARGIN_MS)?;
    if pcm16k.len() - (end - start) < TRIM_MIN_SAVED_SAMPLES {
        return None;
    }
    let wav = aqua_audio_adapter::resample::mono_wav_bytes(&pcm16k[start..end], 16_000).ok()?;
    let path = scratch.join("speech.wav");
    std::fs::write(&path, wav).ok()?;
    info!(
        "recording trimmed to the speech: kept {:.1} s of {:.1} s (from {:.1} s)",
        (end - start) as f32 / 16_000.0,
        pcm16k.len() as f32 / 16_000.0,
        start as f32 / 16_000.0
    );
    Some(path)
}
