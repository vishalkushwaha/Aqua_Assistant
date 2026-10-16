use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::info;

/// Voice activity detection. Despite the name (kept for compatibility) this is an ENERGY detector: loudness plus
/// how noise-like the frame is. The Silero ONNX model is not loaded, even if `silero_vad.onnx` exists, and
/// `engine()` says so. It is a gate for "is anything being said at all", not a speech classifier.
pub struct SileroVadDetector {
    model_path: PathBuf,
    is_model_present: bool,
    threshold: f32,
}

impl SileroVadDetector {
    /// Silero VAD ONNX Engine — optional deep neural voice activity detector under `data/models/vad/silero_vad.onnx`.
    pub fn new<P: AsRef<Path>>(model_dir: P) -> Result<Self> {
        let model_dir = model_dir.as_ref();
        let model_path = model_dir.join("silero_vad.onnx");
        let is_model_present = model_path.is_file();

        if is_model_present {
            info!("Found {:?}, but the Silero model is not loaded: the energy-based detector is in use", model_path);
        }

        Ok(Self {
            model_path,
            is_model_present,
            threshold: 0.5,
        })
    }

    /// Evaluates probability of speech in 16kHz PCM audio frame (0.0 to 1.0 probability)
    pub fn speech_probability(&self, pcm_frame: &[f32]) -> f32 {
        if pcm_frame.is_empty() {
            return 0.0;
        }

        // Calculate speech spectral features & energy for neural approximation
        let energy: f32 = pcm_frame.iter().map(|&s| s * s).sum::<f32>() / pcm_frame.len() as f32;
        let rms = energy.sqrt();

        // Zero-crossing rate calculation for voice vs unvoiced noise separation
        let mut zcr_count = 0;
        for i in 1..pcm_frame.len() {
            if (pcm_frame[i] >= 0.0 && pcm_frame[i - 1] < 0.0) || (pcm_frame[i] < 0.0 && pcm_frame[i - 1] >= 0.0) {
                zcr_count += 1;
            }
        }
        let zcr = zcr_count as f32 / pcm_frame.len() as f32;

        // Neural probability approximation model based on spectral envelope & RMS energy
        // Loudness decides; a high zero-crossing rate (hiss-like) pulls the score down. Silence scores 0.
        let loudness = (rms * 30.0).clamp(0.0, 1.0);
        let tonal = (1.0 - zcr).clamp(0.5, 1.0);
        loudness * tonal
    }

    pub fn is_speech(&self, pcm_frame: &[f32]) -> bool {
        self.speech_probability(pcm_frame) >= self.threshold
    }

    /// Which detector actually runs.
    pub fn engine(&self) -> &'static str {
        "energy"
    }

    pub fn is_model_present(&self) -> bool {
        self.is_model_present
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }
}
