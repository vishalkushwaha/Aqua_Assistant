use crate::mic_capture::MicCapture;
use crate::noise_cancellation::NoiseSuppressor;
use crate::silero_vad::SileroVadDetector;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub struct VoiceStandbyPipeline {
    mic: MicCapture,
    suppressor: NoiseSuppressor,
    silero_vad: Option<SileroVadDetector>,
    is_active: bool,
    /// Exponential Moving Average (EMA) tracking of background ambient noise floor
    ambient_noise_floor: std::sync::atomic::AtomicU32, // Bit-cast f32 for thread-safe atomic update
}

impl VoiceStandbyPipeline {
    pub fn new<P: AsRef<Path>>(model_dir: P) -> Result<Self> {
        let mic = MicCapture::new()?;
        let suppressor = NoiseSuppressor::new(&model_dir)?;
        let silero_vad = SileroVadDetector::new(&model_dir).ok();

        info!(
            "Voice Standby Pipeline active at {}Hz sample rate (Adaptive VAD + Silero ONNX ready)",
            mic.sample_rate()
        );

        Ok(Self {
            mic,
            suppressor,
            silero_vad,
            is_active: true,
            ambient_noise_floor: std::sync::atomic::AtomicU32::new(0.005f32.to_bits()),
        })
    }

    /// Local Automatic Gain Control (AGC): Peak volume normalization and soft knee limiter.
    pub fn normalize_agc_frame(&self, frame: &[f32]) -> Vec<f32> {
        if frame.is_empty() {
            return Vec::new();
        }

        // Sanitize non-finite inputs (NaN, Infinity) to 0.0
        let sanitized: Vec<f32> = frame
            .iter()
            .map(|&s| if s.is_finite() { s } else { 0.0 })
            .collect();

        let energy: f32 = sanitized.iter().map(|&s| s * s).sum::<f32>() / sanitized.len() as f32;
        let rms = energy.sqrt();

        if rms < 0.0001 {
            return sanitized;
        }

        let target_rms = 0.15f32;
        let gain = (target_rms / rms).clamp(1.0, 4.0);

        sanitized
            .iter()
            .map(|&sample| {
                let scaled = sample * gain;
                let limited = if scaled > 0.98 {
                    0.98 + (scaled - 0.98) * 0.1
                } else if scaled < -0.98 {
                    -0.98 + (scaled + 0.98) * 0.1
                } else {
                    scaled
                };
                limited.clamp(-1.0, 1.0)
            })
            .collect()
    }

    /// Adaptive & Silero Neural Voice Activity Detection (VAD): Tracks dynamic noise floor + neural spectral probability.
    pub fn is_speech_active(&self, pcm_frame: &[f32]) -> bool {
        if pcm_frame.is_empty() {
            return false;
        }

        // Apply Local Automatic Gain Control (AGC) to ensure consistent volume scaling
        let agc_frame = self.normalize_agc_frame(pcm_frame);

        // 1. Silero Neural VAD evaluation if model present
        if let Some(ref silero) = self.silero_vad {
            if silero.is_model_present() {
                return silero.is_speech(&agc_frame);
            }
        }

        // 2. Fallback to Dynamic Adaptive EMA Energy Thresholding
        let energy: f32 = agc_frame.iter().map(|&s| s * s).sum::<f32>() / agc_frame.len() as f32;
        let current_rms = energy.sqrt();

        let current_ambient_bits = self.ambient_noise_floor.load(std::sync::atomic::Ordering::Relaxed);
        let ambient_rms = f32::from_bits(current_ambient_bits);

        let speech_margin = 0.012f32;
        let adaptive_threshold = (ambient_rms + speech_margin).clamp(0.010, 0.150);

        if current_rms < adaptive_threshold {
            let new_ambient = ambient_rms * 0.95 + current_rms * 0.05;
            self.ambient_noise_floor.store(new_ambient.to_bits(), std::sync::atomic::Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    /// Pull next audio chunk from microphone, process through noise suppressor, apply VAD, and return clean speech audio frame.
    pub fn read_clean_frame(&self) -> Option<Vec<f32>> {
        if !self.is_active {
            return None;
        }

        if let Ok(raw_frame) = self.mic.receiver().try_recv() {
            let clean_frame = self.suppressor.process_frame(&raw_frame);
            if self.is_speech_active(&clean_frame) {
                Some(clean_frame)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn is_suppressor_loaded(&self) -> bool {
        self.suppressor.is_model_loaded()
    }
}
