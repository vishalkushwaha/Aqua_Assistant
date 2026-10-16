//! Voice biometrics: turn a few seconds of speech into a voice fingerprint and compare fingerprints.
//!
//! The default speaker model is WeSpeaker ResNet34-LM (`speaker_wespeaker_resnet34_lm.onnx`, see
//! `scripts/fetch_speaker_model.ps1`). It reads an 80-band log-mel spectrogram ([`crate::speaker_features`]) and answers
//! with a 256-number fingerprint. An older model that reads raw samples (`ecapa_tdnn.onnx`) is still accepted if it is the
//! only one present. The size of the fingerprint is learned from the model itself, so a voice enrolled with one model is
//! never compared against fingerprints of another size (it must be enrolled again).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tract_onnx::prelude::*;
use tracing::{info, warn};

use crate::speaker_features::{normalised_fbank, MEL_BANDS};

/// The compiled model; the type is long, so it is named once.
type Plan = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

/// Model files looked for in the biometrics folder, best first. The first takes spectrogram features, the second raw samples.
pub const MODEL_FILES: [&str; 2] = ["speaker_wespeaker_resnet34_lm.onnx", "ecapa_tdnn.onnx"];

/// Cosine similarity at or above which a voice is accepted. Measured with this model: the same speaker saying different
/// sentences scored 0.69 to 0.83 (mean 0.79) and different speakers up to 0.55 (mean 0.13), so 0.55 keeps the owner in and
/// strangers out with room to spare for the ordinary variation of a real person (a cold, a different microphone). It can be
/// changed with `AQUA_VOICE_MATCH_THRESHOLD` (0.20 to 0.95): higher is stricter.
pub const DEFAULT_MATCH_THRESHOLD: f32 = 0.55;

/// Shortest recording a fingerprint is made from (half a second of speech); anything shorter is too little to tell people
/// apart, and is refused rather than guessed from.
pub const MIN_SAMPLES: usize = 8_000;
/// Longest stretch used for one fingerprint (8 s): more adds time (about a quarter of a second per second of audio), not
/// accuracy. From a longer recording the loudest 8 s are used, not the first, which may be silence before the person starts.
pub const MAX_SAMPLES: usize = 8 * 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    /// `[1, frames, 80]` log-mel features (WeSpeaker, 3D-Speaker, NeMo style models).
    Features,
    /// `[1, samples]` raw 16 kHz audio (the earlier, project-specific export).
    RawSamples,
}

/// What an enrolment recording produced.
#[derive(Debug, Clone)]
pub struct EnrolmentPrint {
    pub embedding: Vec<f32>,
    /// How many 5-second parts were fingerprinted.
    pub parts: usize,
    /// How alike those parts were (mean cosine similarity between them; 1.0 when there was only one part).
    pub consistency: f32,
}

/// At most this many parts are used to enrol (30 s of speech is plenty, and each part costs about a second).
const MAX_ENROLMENT_PARTS: usize = 6;
/// The parts of an enrolment recording must agree at least this much. Parts of one person's speech measured 0.66 to 0.83
/// against each other with this model; two different voices, up to 0.45.
pub const MIN_ENROLMENT_CONSISTENCY: f32 = 0.50;

pub struct VoiceBiometricsEngine {
    model_path: PathBuf,
    is_model_loaded: bool,
    master_fingerprint: Option<Vec<f32>>,
    match_threshold: f32,
    model: Option<Arc<Plan>>,
    input_kind: InputKind,
    /// How many numbers one fingerprint has, learned by running the model once at start.
    embedding_dim: Option<usize>,
}

fn threshold_from_env() -> f32 {
    std::env::var("AQUA_VOICE_MATCH_THRESHOLD")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.20, 0.95))
        .unwrap_or(DEFAULT_MATCH_THRESHOLD)
}

/// A second of a made-up voice-like signal, to find out what the model answers with.
fn probe_signal() -> Vec<f32> {
    (0..16_000)
        .map(|i| {
            let t = i as f32 / 16_000.0;
            0.2 * (2.0 * std::f32::consts::PI * 140.0 * t).sin() + 0.1 * (2.0 * std::f32::consts::PI * 910.0 * t).sin() + 0.05 * (2.0 * std::f32::consts::PI * 2300.0 * t).sin()
        })
        .collect()
}

impl VoiceBiometricsEngine {
    /// Initialise the engine from the models in `model_dir`. A missing or unusable model is not an error: the engine then
    /// reports itself as not loaded and every extraction fails with a clear message.
    pub fn new<P: AsRef<Path>>(model_dir: P) -> Result<Self> {
        let model_dir = model_dir.as_ref();
        let found = MODEL_FILES.iter().enumerate().map(|(i, f)| (i, model_dir.join(f))).find(|(_, p)| p.is_file());
        let (idx, target_model) = found.unwrap_or_else(|| (0, model_dir.join(MODEL_FILES[0])));
        let input_kind = if idx == 0 { InputKind::Features } else { InputKind::RawSamples };

        info!("Initializing the speaker model from {:?}", target_model);
        let mut engine = Self {
            model_path: target_model.clone(),
            is_model_loaded: false,
            master_fingerprint: None,
            match_threshold: threshold_from_env(),
            model: None,
            input_kind,
            embedding_dim: None,
        };

        if !target_model.is_file() {
            warn!("No speaker model found in {:?} (expected {}). Run scripts/fetch_speaker_model.ps1. Voice recognition is disabled.", model_dir, MODEL_FILES[0]);
            return Ok(engine);
        }
        match load_plan(&target_model) {
            Ok(plan) => {
                engine.model = Some(Arc::new(plan));
                // Run it once: this proves the model works here and tells us how big its answer is.
                match engine.run_model(&probe_signal()) {
                    Ok(e) => {
                        engine.embedding_dim = Some(e.len());
                        engine.is_model_loaded = true;
                        info!("Speaker model ready ({} numbers per voice, match threshold {:.2}).", e.len(), engine.match_threshold);
                    }
                    Err(e) => {
                        engine.model = None;
                        warn!("The speaker model loaded but could not be run: {e:#}");
                    }
                }
            }
            Err(e) => warn!("The speaker model could not be loaded: {e:#}"),
        }
        Ok(engine)
    }

    /// Register or re-calibrate the Master User voice fingerprint embedding.
    pub fn register_master_fingerprint(&mut self, embedding: Vec<f32>) -> Result<()> {
        if embedding.is_empty() || embedding.iter().any(|v| !v.is_finite()) || embedding.iter().all(|v| *v == 0.0) {
            anyhow::bail!("Invalid embedding.");
        }
        if let Some(dim) = self.embedding_dim {
            if embedding.len() != dim {
                anyhow::bail!(
                    "this voice was recorded with a different speaker model ({} numbers; the current one makes {dim})",
                    embedding.len()
                );
            }
        }
        info!("Successfully registered Master User voice fingerprint.");
        self.master_fingerprint = Some(embedding);
        Ok(())
    }

    /// Extract the voice fingerprint (unit length) from 16 kHz mono PCM.
    pub fn extract_embedding(&self, pcm_16k_samples: &[f32]) -> Result<Vec<f32>> {
        if pcm_16k_samples.is_empty() {
            anyhow::bail!("Audio frame is empty.");
        }
        if self.model.is_none() {
            anyhow::bail!("ONNX model not loaded. Cannot perform neural inference.");
        }
        if pcm_16k_samples.len() < MIN_SAMPLES {
            anyhow::bail!(
                "That is too short to recognise a voice ({:.1} s; at least {:.1} s of speech is needed).",
                pcm_16k_samples.len() as f32 / 16_000.0,
                MIN_SAMPLES as f32 / 16_000.0
            );
        }
        self.run_model(loudest_window(pcm_16k_samples, MAX_SAMPLES))
    }

    fn run_model(&self, samples: &[f32]) -> Result<Vec<f32>> {
        let model = self.model.as_ref().context("ONNX model not loaded")?;
        let input: Tensor = match self.input_kind {
            InputKind::Features => {
                let (frames, flat) = normalised_fbank(samples)?;
                tract_ndarray::Array3::from_shape_vec((1, frames, MEL_BANDS), flat).context("Failed to shape the features")?.into_tensor()
            }
            InputKind::RawSamples => tract_ndarray::Array2::from_shape_vec((1, samples.len()), samples.to_vec()).context("Failed to create input array")?.into_tensor(),
        };
        let result = model.run(tvec!(input.into()))?;
        let output = result[0].to_array_view::<f32>()?;
        let mut embedding: Vec<f32> = output.iter().copied().collect();

        // L2 Normalize embedding vector
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        // A NaN or all-zero vector is not a voiceprint of anyone: stored, it would make every later check fail.
        if !norm.is_finite() || norm <= 0.0 || embedding.iter().any(|v| !v.is_finite()) {
            anyhow::bail!("The speaker model returned no usable voice features (is the recording silent or damaged?).");
        }
        for val in embedding.iter_mut() {
            *val /= norm;
        }
        Ok(embedding)
    }

    /// Make the fingerprint to enrol from a longer recording. The speech is cut into 5-second parts (the loudest six at most),
    /// each is fingerprinted, and the fingerprints are averaged; how much the parts agree with each other is returned too.
    /// A recording whose parts do not sound like one voice (two people, a radio, a muted microphone and noise) is refused:
    /// enrolling it would lock in a fingerprint that matches nobody, in particular not its owner.
    pub fn enrolment_fingerprint(&self, samples: &[f32]) -> Result<EnrolmentPrint> {
        if self.model.is_none() {
            anyhow::bail!("ONNX model not loaded. Cannot perform neural inference.");
        }
        const PART: usize = 5 * 16_000;
        let mut parts: Vec<&[f32]> = samples.chunks(PART).filter(|c| c.len() >= 2 * 16_000 || samples.len() < 2 * 16_000 + PART / 2).collect();
        let energy = |c: &[f32]| c.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / c.len().max(1) as f64;
        parts.retain(|c| energy(c) > 1e-5); // not silence
        parts.sort_by(|a, b| energy(b).partial_cmp(&energy(a)).unwrap_or(std::cmp::Ordering::Equal));
        parts.truncate(MAX_ENROLMENT_PARTS);
        if parts.is_empty() {
            anyhow::bail!("There is no speech in that recording to learn a voice from.");
        }
        let prints: Vec<Vec<f32>> = parts.iter().map(|p| self.extract_embedding(p)).collect::<Result<_>>()?;
        let mut mean = vec![0.0f32; prints[0].len()];
        for e in &prints {
            for (m, v) in mean.iter_mut().zip(e) {
                *m += v;
            }
        }
        let norm = mean.iter().map(|v| v * v).sum::<f32>().sqrt();
        if !norm.is_finite() || norm <= 0.0 {
            anyhow::bail!("The speaker model returned no usable voice features (is the recording silent or damaged?).");
        }
        mean.iter_mut().for_each(|v| *v /= norm);
        let mut pairs = Vec::new();
        for i in 0..prints.len() {
            for j in i + 1..prints.len() {
                pairs.push(cosine_similarity(&prints[i], &prints[j]));
            }
        }
        let consistency = if pairs.is_empty() { 1.0 } else { pairs.iter().sum::<f32>() / pairs.len() as f32 };
        if consistency < MIN_ENROLMENT_CONSISTENCY {
            anyhow::bail!(
                "The parts of that recording do not sound like one voice ({:.0}% alike; at least {:.0}% is needed). Record again alone, in a quiet room, speaking naturally.",
                consistency * 100.0,
                MIN_ENROLMENT_CONSISTENCY * 100.0
            );
        }
        Ok(EnrolmentPrint { embedding: mean, parts: prints.len(), consistency })
    }

    /// Verify incoming speaker audio frame against registered master fingerprint.
    /// Returns (is_authorized: bool, cosine_similarity: f32)
    pub fn verify_speaker(&mut self, speaker_embedding: &[f32]) -> (bool, f32) {
        let master = match &self.master_fingerprint {
            Some(m) => m,
            None => {
                warn!("No master voice fingerprint registered. Rejecting speaker.");
                return (false, 0.0);
            }
        };
        if speaker_embedding.len() != master.len() {
            return (false, 0.0);
        }
        let similarity = cosine_similarity(master, speaker_embedding);
        (similarity >= self.match_threshold, similarity)
    }

    pub fn is_master_registered(&self) -> bool {
        self.master_fingerprint.is_some()
    }

    pub fn is_model_loaded(&self) -> bool {
        self.is_model_loaded
    }

    pub fn match_threshold(&self) -> f32 {
        self.match_threshold
    }

    /// How many numbers one fingerprint has (known once the model has loaded).
    pub fn embedding_dim(&self) -> Option<usize> {
        self.embedding_dim
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }
}

fn load_plan(path: &Path) -> Result<Plan> {
    // The number of frames differs from recording to recording, so the plan is built for any length and not specialised.
    Ok(tract_onnx::onnx().model_for_path(path)?.into_optimized()?.into_runnable()?)
}

/// The `len` samples of `samples` with the most energy (the whole of it if it is not longer), searched in steps of one second.
fn loudest_window(samples: &[f32], len: usize) -> &[f32] {
    if samples.len() <= len {
        return samples;
    }
    let step = 16_000;
    let mut best = (0usize, f64::MIN);
    let mut start = 0;
    while start + len <= samples.len() {
        let e: f64 = samples[start..start + len].iter().map(|v| (*v as f64) * (*v as f64)).sum();
        if e > best.1 {
            best = (start, e);
        }
        start += step;
    }
    &samples[best.0..best.0 + len]
}

/// Cosine similarity of two vectors of the same length (0 if either has no length).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let (na, nb) = (a.iter().map(|x| x * x).sum::<f32>().sqrt(), b.iter().map(|x| x * x).sum::<f32>().sqrt());
    if na > 0.0 && nb > 0.0 { dot / (na * nb) } else { 0.0 }
}

/// Decode a WAV to the shape the speaker model expects: 16 kHz, mono (any input rate, channel count or bit depth is
/// converted, and a damaged file is an error, not a shorter recording).
pub fn extract_pcm16_samples(wav_bytes: &[u8]) -> Result<Vec<f32>> {
    crate::resample::wav_to_16k_mono(wav_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_of_identical_opposite_and_unrelated_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
        assert!((cosine_similarity(&a, &[-1.0, -2.0, -3.0]) + 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &a[..2]), 0.0);
    }

    #[test]
    fn from_a_long_recording_the_loudest_stretch_is_used() {
        let mut long = vec![0.001f32; 16_000 * 20];
        for v in long.iter_mut().skip(16_000 * 11).take(16_000 * 8) {
            *v = 0.3;
        }
        let w = loudest_window(&long, MAX_SAMPLES);
        assert_eq!(w.len(), MAX_SAMPLES);
        assert!(w.iter().all(|v| *v == 0.3), "the loud part, not the quiet start");
        let short = vec![0.1f32; 16_000];
        assert_eq!(loudest_window(&short, MAX_SAMPLES).len(), 16_000);
    }

    #[test]
    fn the_threshold_default_and_its_override_are_bounded() {
        std::env::remove_var("AQUA_VOICE_MATCH_THRESHOLD");
        assert_eq!(threshold_from_env(), DEFAULT_MATCH_THRESHOLD);
        std::env::set_var("AQUA_VOICE_MATCH_THRESHOLD", "0.7");
        assert_eq!(threshold_from_env(), 0.7);
        std::env::set_var("AQUA_VOICE_MATCH_THRESHOLD", "5");
        assert_eq!(threshold_from_env(), 0.95, "never accepting nobody");
        std::env::set_var("AQUA_VOICE_MATCH_THRESHOLD", "-1");
        assert_eq!(threshold_from_env(), 0.20, "never accepting everybody");
        std::env::set_var("AQUA_VOICE_MATCH_THRESHOLD", "abc");
        assert_eq!(threshold_from_env(), DEFAULT_MATCH_THRESHOLD);
        std::env::remove_var("AQUA_VOICE_MATCH_THRESHOLD");
    }

    #[test]
    fn without_a_model_the_engine_says_so_and_refuses_to_extract() {
        let dir = std::env::temp_dir().join(format!("aqua_nomodel_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let e = VoiceBiometricsEngine::new(&dir).unwrap();
        assert!(!e.is_model_loaded() && e.embedding_dim().is_none());
        assert!(e.extract_embedding(&vec![0.1; 16_000]).unwrap_err().to_string().contains("not loaded"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
