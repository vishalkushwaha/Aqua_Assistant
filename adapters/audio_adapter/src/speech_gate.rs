//! A cheap check, before any speech model runs, for "is there a person talking in this clip?".
//!
//! Loudness alone cannot tell: wind, a fan, and the clicks of typing are loud, and Whisper, given noise, invents words for it
//! ("Hello.", "Hello everyone.", "Thank you."), which Aqua then answers ("Hello! How can I assist you today?"). Speech has
//! something noise does not: **voiced sound**, a steady pitch (a periodic waveform) that holds for at least a syllable. Wind
//! is smooth but not periodic, a fan is a hum without a pitch that moves like a voice, typing is a few milliseconds of click.
//! So a clip passes only if it holds a stretch of periodic, pitch-continuous sound a person's voice makes (70 to 500 Hz).
//!
//! This is the quick first filter (a few milliseconds for a clip of seconds, no process started). It is deliberately lenient:
//! it only rejects a clip with no voiced stretch at all. The trained voice-activity model inside Whisper (`--vad`, see
//! `whisper_cli`) makes the final call for what passes.

/// Frame and hop at the 8 kHz the pitch search runs at (the clip is averaged down from 16 kHz).
const RATE: f32 = 8000.0;
const FRAME: usize = 256; // 32 ms
const HOP: usize = 128; // 16 ms
/// Pitch search range in samples at 8 kHz: 500 Hz down to about 70 Hz.
const MIN_LAG: usize = 16;
const MAX_LAG: usize = 114;
/// A voiced stretch: frames whose pitch period differs by no more than this from the one before.
const PITCH_STEP: f32 = 0.22;

/// How strict the gate is. Measured on 247 speech clips (11 spoken commands, JFK and earlier recordings, alone and under wind,
/// typing and a fan at 20, 10, 5 and 0 dB signal to noise) and 72 noise clips (wind, typing, a fan; six variations at four
/// levels each):
/// - `Lenient`: no speech clip lost, 1 of 72 noise clips let through. For use with the voice-activity model behind it, which
///   throws that one out.
/// - `Strict`: 4 of the 247 speech clips lost (all at 0 to 5 dB, noise as loud as the voice), no noise let through. For use when
///   there is no voice-activity model to catch what the gate lets by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    Lenient,
    Strict,
}

struct Thresholds {
    /// How periodic a frame must be to count as voiced (normalised autocorrelation, 0 to 1), and at twice the period.
    periodic: f32,
    periodic_at_double: f32,
    /// Voiced sound needed to call a clip speech: in all, and in one unbroken stretch.
    min_voiced_ms: u32,
    min_run_ms: u32,
}

impl Strictness {
    fn thresholds(self) -> Thresholds {
        match self {
            Strictness::Lenient => Thresholds { periodic: 0.35, periodic_at_double: 0.10, min_voiced_ms: 80, min_run_ms: 32 },
            Strictness::Strict => Thresholds { periodic: 0.40, periodic_at_double: 0.15, min_voiced_ms: 80, min_run_ms: 48 },
        }
    }
}

/// A frame is "active" (loud enough to be worth looking at) above this, or above this times the quietest tenth of the clip.
const ACTIVE_FLOOR_RMS: f32 = 0.002;
const ACTIVE_OVER_NOISE: f32 = 1.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Speech,
    /// Not speech, and why: "no voiced sound" (noise, wind, a fan, clicks) or "too short".
    NotSpeech(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeechReport {
    pub verdict: Verdict,
    /// Milliseconds of voiced sound in all, and in the longest unbroken stretch.
    pub voiced_ms: u32,
    pub longest_run_ms: u32,
    /// Milliseconds of clip that was loud enough to look at.
    pub active_ms: u32,
}

impl SpeechReport {
    pub fn is_speech(&self) -> bool {
        self.verdict == Verdict::Speech
    }
}

/// `AQUA_SPEECH_GATE=off` turns the gate off (everything is then judged by the voice-activity model alone).
fn gate_enabled() -> bool {
    !std::env::var("AQUA_SPEECH_GATE").is_ok_and(|v| v.eq_ignore_ascii_case("off") || v == "0")
}

/// Judge 16 kHz mono samples (-1.0 to 1.0).
pub fn analyze(samples16k: &[f32], strictness: Strictness) -> SpeechReport {
    analyze_with(samples16k, gate_enabled(), strictness)
}

fn analyze_with(samples16k: &[f32], enabled: bool, strictness: Strictness) -> SpeechReport {
    let t = strictness.thresholds();
    let mk = |verdict, voiced_ms, longest_run_ms, active_ms| SpeechReport { verdict, voiced_ms, longest_run_ms, active_ms };
    if !enabled {
        return mk(Verdict::Speech, 0, 0, 0);
    }
    let Some(s) = scan(samples16k, &t) else {
        return mk(Verdict::NotSpeech("too short"), 0, 0, 0);
    };
    let (voiced_ms, longest_ms, active_ms) = (s.voiced_frames * s.hop_ms, s.longest * s.hop_ms, s.active_frames * s.hop_ms);
    let verdict = if voiced_ms >= t.min_voiced_ms && longest_ms >= t.min_run_ms { Verdict::Speech } else { Verdict::NotSpeech("no voiced sound") };
    mk(verdict, voiced_ms, longest_ms, active_ms)
}

/// What the frame-by-frame analysis of a clip found: how much of it is voiced, and where each unbroken voiced stretch is.
struct Scan {
    hop_ms: u32,
    voiced_frames: u32,
    longest: u32,
    active_frames: u32,
    /// Each unbroken voiced stretch as (first frame, last frame); a frame is `HOP` samples at 8 kHz.
    runs: Vec<(usize, usize)>,
}

/// The analysis the gate and [`speech_span`] share, so that they cannot disagree about where the voice is. `None` when the clip is
/// too short to judge.
fn scan(samples16k: &[f32], t: &Thresholds) -> Option<Scan> {
    if samples16k.len() < 16_000 / 5 {
        return None;
    }
    // 16 kHz to 8 kHz by averaging pairs (a crude low-pass, which is all a pitch search needs), then a pre-emphasis that takes
    // the weight off the rumble below 100 Hz so wind does not look like a pitch.
    let half: Vec<f32> = samples16k.chunks_exact(2).map(|p| (p[0] + p[1]) * 0.5).collect();
    let mut y = Vec::with_capacity(half.len());
    let mut prev = 0.0f32;
    for &x in &half {
        y.push(x - 0.95 * prev);
        prev = x;
    }
    let frames = if half.len() >= FRAME { (half.len() - FRAME) / HOP + 1 } else { 0 };
    if frames == 0 {
        return None;
    }
    // Loudness of each frame, and the level of the quietest tenth, which is the clip's own noise floor.
    let rms: Vec<f32> = (0..frames).map(|f| rms_of(&half[f * HOP..f * HOP + FRAME])).collect();
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor = sorted[frames / 10];
    let active_above = ACTIVE_FLOOR_RMS.max(floor * ACTIVE_OVER_NOISE);

    let hop_ms = (HOP as f32 / RATE * 1000.0) as u32;
    let (mut voiced_frames, mut run, mut longest, mut active_frames) = (0u32, 0u32, 0u32, 0u32);
    let mut last_lag: Option<usize> = None;
    let mut gap = 0u32;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut current: Option<(usize, usize)> = None;
    for f in 0..frames {
        if rms[f] < active_above {
            gap += 1;
            if gap > 1 {
                run = 0;
                last_lag = None;
                runs.extend(current.take());
            }
            continue;
        }
        active_frames += 1;
        match pitch(&y[f * HOP..f * HOP + FRAME], t) {
            Some(lag) => {
                let continues = last_lag.map_or(true, |l| (lag as f32 - l as f32).abs() / l as f32 <= PITCH_STEP);
                run = if continues { run + 1 } else { 1 };
                last_lag = Some(lag);
                voiced_frames += 1;
                gap = 0;
                longest = longest.max(run);
                current = match current {
                    Some((first, _)) if continues => Some((first, f)),
                    other => {
                        runs.extend(other);
                        Some((f, f))
                    }
                };
            }
            None => {
                gap += 1;
                if gap > 1 {
                    run = 0;
                    last_lag = None;
                    runs.extend(current.take());
                }
            }
        }
    }
    runs.extend(current.take());
    Some(Scan { hop_ms, voiced_frames, longest, active_frames, runs })
}

/// Where the speech is in a clip: the first and the last stretch of voiced sound, widened by `margin_ms` on each side (a word
/// can start or end with a sound that has no pitch, "s" or "f", and the speech model reads better with a little room), as the
/// range of 16 kHz samples to keep. `None` when there is no voice to anchor on, which includes every clip the gate would refuse.
/// Stretches shorter than 80 ms are not trusted as the start or the end (a click or a cough can flicker as voiced), and the
/// result is never shorter than one second.
pub fn speech_span(samples16k: &[f32], margin_ms: u32) -> Option<(usize, usize)> {
    let t = Strictness::Lenient.thresholds();
    let s = scan(samples16k, &t)?;
    let frames_of = |ms: u32| (ms / s.hop_ms).max(1) as usize;
    let anchors = |min_frames: usize| s.runs.iter().copied().filter(move |(a, b)| b - a + 1 >= min_frames).collect::<Vec<_>>();
    let mut anchored = anchors(frames_of(80));
    if anchored.is_empty() {
        // A very short word has no stretch that long: the clip is judged on its longest.
        anchored = anchors(frames_of(t.min_run_ms));
    }
    let (first, last) = (anchored.first()?, anchored.last()?);
    // A frame at 8 kHz is `HOP` samples apart and `FRAME` long; the clip is at 16 kHz.
    let (voice_start, voice_end) = (first.0 * HOP * 2, (last.1 * HOP + FRAME) * 2);
    let margin = margin_ms as usize * 16;
    let (mut start, mut end) = (voice_start.saturating_sub(margin), (voice_end + margin).min(samples16k.len()));
    let shortest = 16_000;
    if end - start < shortest {
        let missing = shortest - (end - start);
        start = start.saturating_sub(missing / 2);
        end = (end + missing - missing / 2).min(samples16k.len());
        if end - start < shortest {
            start = end.saturating_sub(shortest);
        }
    }
    Some((start, end))
}

fn rms_of(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// The pitch period (in samples at 8 kHz) of a frame, if it is clearly periodic: the best normalised autocorrelation in the
/// voice range must be strong, and the sound must repeat at twice the period too (true periodic sound does; smooth noise does not).
fn pitch(frame: &[f32], t: &Thresholds) -> Option<usize> {
    let n = frame.len();
    let energy0: f32 = frame.iter().map(|v| v * v).sum();
    if energy0 < 1e-8 {
        return None;
    }
    let nccf = |lag: usize| -> f32 {
        let (mut num, mut e1, mut e2) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..n - lag {
            num += frame[i] * frame[i + lag];
            e1 += frame[i] * frame[i];
            e2 += frame[i + lag] * frame[i + lag];
        }
        num / (e1 * e2).sqrt().max(1e-9)
    };
    let (mut best, mut best_lag) = (0.0f32, 0usize);
    for lag in MIN_LAG..=MAX_LAG {
        let c = nccf(lag);
        if c > best {
            best = c;
            best_lag = lag;
        }
    }
    if best < t.periodic {
        return None;
    }
    // Take the shortest lag that is nearly as good (the true period, not a multiple of it).
    for lag in MIN_LAG..best_lag {
        if nccf(lag) >= best * 0.92 {
            best_lag = lag;
            break;
        }
    }
    let double = best_lag * 2;
    (double + 8 < n && nccf(double) >= t.periodic_at_double).then_some(best_lag)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 16_000.0;

    /// A crude voice: harmonics of a gliding pitch, a few syllable-long bursts.
    fn voice(seconds: f32, f0: f32) -> Vec<f32> {
        (0..(seconds * SR) as usize)
            .map(|i| {
                let t = i as f32 / SR;
                let f = f0 * (1.0 + 0.08 * (2.0 * std::f32::consts::PI * 2.0 * t).sin());
                let phase = 2.0 * std::f32::consts::PI * f * t;
                let syllables = 0.5 + 0.5 * (2.0 * std::f32::consts::PI * 3.5 * t).sin();
                let harmonics: f32 = (1..=8).map(|h| (phase * h as f32).sin() / h as f32).sum();
                0.12 * syllables * harmonics
            })
            .collect()
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
        }
    }

    fn lowpass(x: &[f32], fc: f32) -> Vec<f32> {
        let a = (-2.0 * std::f32::consts::PI * fc / SR).exp();
        let mut y = 0.0;
        x.iter().map(|v| { y = a * y + (1.0 - a) * v; y }).collect()
    }

    fn scaled(x: Vec<f32>, rms: f32) -> Vec<f32> {
        let r = rms_of(&x).max(1e-9);
        x.into_iter().map(|v| v * rms / r).collect()
    }

    fn wind(seconds: f32, seed: u64, level: f32) -> Vec<f32> {
        let mut r = Rng(seed);
        let n = (seconds * SR) as usize;
        let rumble = lowpass(&(0..n).map(|_| r.next()).collect::<Vec<_>>(), 120.0);
        let hiss = lowpass(&(0..n).map(|_| r.next()).collect::<Vec<_>>(), 2500.0);
        scaled((0..n).map(|i| (rumble[i] * 6.0 + hiss[i] * 1.2) * (0.4 + 0.6 * (i as f32 / SR * 0.7).sin().abs())).collect(), level)
    }

    fn typing(seconds: f32, seed: u64, level: f32) -> Vec<f32> {
        let mut r = Rng(seed);
        let n = (seconds * SR) as usize;
        let mut out: Vec<f32> = (0..n).map(|_| r.next() * 0.02).collect();
        let mut t = (0.2 * SR) as usize;
        while t + 800 < n {
            for j in 0..(0.025 * SR) as usize {
                let env = (-(j as f32) / (SR * 0.004)).exp();
                out[t + j] += r.next() * env * 4.0;
            }
            t += ((0.07 + (r.next().abs()) * 0.25) * SR) as usize;
        }
        scaled(out, level)
    }

    fn hum(seconds: f32, level: f32) -> Vec<f32> {
        let n = (seconds * SR) as usize;
        scaled((0..n).map(|i| [(100.0, 1.0), (200.0, 0.6), (300.0, 0.3)].iter().map(|(f, a)| a * (2.0 * std::f32::consts::PI * f * i as f32 / SR).sin()).sum()).collect(), level)
    }

    fn analyze_len(x: &[f32]) -> SpeechReport {
        analyze_with(x, true, Strictness::Lenient)
    }

    fn mix(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| x + y).collect()
    }

    #[test]
    fn a_voice_is_speech_and_noise_is_not() {
        assert!(analyze_len(&voice(2.0, 130.0)).is_speech());
        assert!(analyze_len(&voice(1.5, 220.0)).is_speech(), "a higher voice");
        for (name, clip) in [("wind", wind(6.0, 1, 0.05)), ("loud wind", wind(6.0, 2, 0.15)), ("typing", typing(6.0, 3, 0.03)), ("loud typing", typing(6.0, 4, 0.1)), ("a fan hum", hum(6.0, 0.05))] {
            let r = analyze_len(&clip);
            assert!(!r.is_speech(), "{name} was taken for speech: {r:?}");
        }
    }

    #[test]
    fn speech_with_noise_on_top_is_still_speech() {
        let v = voice(3.0, 140.0);
        for (name, noise) in [("typing", typing(3.0, 5, 0.04)), ("wind", wind(3.0, 6, 0.04)), ("hum", hum(3.0, 0.03))] {
            let r = analyze_len(&mix(&v, &noise));
            assert!(r.is_speech(), "speech under {name} was lost: {r:?}");
        }
    }

    #[test]
    fn silence_and_a_blip_are_not_speech() {
        assert!(!analyze_len(&vec![0.0; 16_000]).is_speech());
        assert!(!analyze_len(&vec![0.0005; 16_000]).is_speech());
        assert_eq!(analyze_len(&voice(0.1, 130.0)).verdict, Verdict::NotSpeech("too short"));
        assert_eq!(analyze_len(&[]).verdict, Verdict::NotSpeech("too short"));
    }

    #[test]
    fn the_gate_can_be_switched_off() {
        assert!(analyze_with(&wind(2.0, 9, 0.05), false, Strictness::Lenient).is_speech(), "off: everything goes on to the voice-activity model");
        assert!(!analyze_with(&wind(2.0, 9, 0.05), true, Strictness::Strict).is_speech());
    }

    /// A quiet room: a faint hiss, the noise floor of a clip with nobody talking.
    fn quiet(seconds: f32, seed: u64) -> Vec<f32> {
        let mut r = Rng(seed);
        scaled((0..(seconds * SR) as usize).map(|_| r.next()).collect(), 0.002)
    }

    fn at(seconds: f32) -> usize {
        (seconds * SR) as usize
    }

    #[test]
    fn the_silence_around_speech_is_found_and_the_margins_are_kept() {
        let mut clip = quiet(3.0, 1);
        clip.extend(voice(1.5, 130.0));
        clip.extend(quiet(3.0, 2));
        let (start, end) = speech_span(&clip, 400).expect("there is a voice in it");
        // The voice runs from 3.0 s to 4.5 s; with 400 ms either side, and a little less than that before the first voiced frame.
        assert!((at(2.4)..=at(3.1)).contains(&start), "start at {:.2} s", start as f32 / SR);
        assert!((at(4.5)..=at(5.1)).contains(&end), "end at {:.2} s", end as f32 / SR);
        assert!(end - start < clip.len() / 2, "more than half of the clip was only silence");
    }

    #[test]
    fn a_pause_inside_the_speech_is_part_of_it() {
        let mut clip = quiet(2.0, 3);
        clip.extend(voice(1.0, 120.0));
        clip.extend(quiet(0.9, 4));
        clip.extend(voice(1.0, 150.0));
        clip.extend(quiet(2.0, 5));
        let (start, end) = speech_span(&clip, 400).unwrap();
        assert!(start <= at(2.0) && end >= at(4.9), "both stretches are inside: {:.2} s to {:.2} s", start as f32 / SR, end as f32 / SR);
    }

    #[test]
    fn a_clip_with_no_voice_has_no_span() {
        assert_eq!(speech_span(&wind(4.0, 7, 0.05), 400), None);
        assert_eq!(speech_span(&typing(4.0, 8, 0.05), 400), None);
        assert_eq!(speech_span(&quiet(4.0, 9), 400), None);
        assert_eq!(speech_span(&vec![0.0; at(0.1)], 400), None, "too short to judge");
    }

    #[test]
    fn the_span_never_leaves_the_clip_and_is_never_under_a_second() {
        // Speech from the very start to the very end: the whole clip.
        let all = voice(2.0, 130.0);
        assert_eq!(speech_span(&all, 400), Some((0, all.len())));
        // A short word in the middle of silence still gets a second.
        let mut clip = quiet(3.0, 10);
        clip.extend(voice(0.25, 140.0));
        clip.extend(quiet(3.0, 11));
        let (start, end) = speech_span(&clip, 100).unwrap();
        assert!(end - start >= at(1.0) && end <= clip.len(), "{} samples", end - start);
    }

    #[test]
    fn a_brief_flicker_far_from_the_speech_does_not_stretch_the_span() {
        let mut clip = quiet(0.3, 12);
        clip.extend(voice(0.05, 140.0)); // a click or a cough that flickers as voiced
        clip.extend(quiet(3.0, 13));
        clip.extend(voice(1.5, 130.0));
        clip.extend(quiet(1.0, 14));
        let (start, _) = speech_span(&clip, 400).unwrap();
        assert!(start >= at(2.6), "the span starts at the speech ({:.2} s), not at the flicker", start as f32 / SR);
    }

    /// Real speech, not a synthetic voice: the 11 second recording that comes with whisper.cpp, alone and then with a few seconds of
    /// room noise either side. The voice found must be the same voice wherever it sits (compared without the margin, which the end of
    /// the short recording would cut off), and with the margin the padding must be cut away. Skipped when the sample is not there.
    #[test]
    fn real_speech_is_found_the_same_wherever_it_sits_in_the_recording() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/whisper.cpp/samples/jfk.wav");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipped: the whisper.cpp sample recording is not there");
            return;
        };
        let jfk = crate::resample::wav_to_16k_mono(&bytes).unwrap();
        let alone = speech_span(&jfk, 0).expect("the recording is speech");
        let mut padded = quiet(5.0, 21);
        padded.extend(&jfk);
        padded.extend(quiet(6.0, 22));
        let moved = speech_span(&padded, 0).expect("the recording is speech");
        let shift = at(5.0);
        eprintln!(
            "the voice alone: {:.2} s to {:.2} s of {:.2} s; in the padded recording: {:.2} s to {:.2} s of {:.2} s",
            alone.0 as f32 / SR, alone.1 as f32 / SR, jfk.len() as f32 / SR, moved.0 as f32 / SR, moved.1 as f32 / SR, padded.len() as f32 / SR
        );
        assert!(moved.0.abs_diff(alone.0 + shift) <= at(0.1), "the start moved with the speech");
        assert!(moved.1.abs_diff(alone.1 + shift) <= at(0.1), "the end moved with the speech");
        // With the margin the padding is cut away and the voice is inside what is kept.
        let (kept_from, kept_to) = speech_span(&padded, 400).unwrap();
        assert!(kept_from <= moved.0 && kept_to >= moved.1, "the whole voice is kept");
        assert!(kept_to - kept_from < padded.len() * 6 / 10, "the padding was cut away: kept {:.1} s of {:.1} s", (kept_to - kept_from) as f32 / SR, padded.len() as f32 / SR);
    }
}
