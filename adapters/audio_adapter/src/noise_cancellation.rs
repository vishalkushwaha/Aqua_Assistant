use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::deepfilter_cli::{model_bundle_valid, model_tarball, resolve_cli_binary};

/// How far quiet samples are turned down by the frame gate. Gentle on purpose: the gate cannot tell a faint
/// consonant from hiss, so it must never erase what it is unsure about.
const DEFAULT_ATTENUATION_DB: f32 = 15.0;

/// Which processing a denoise call really used. Only `DeepFilterNet` is neural noise suppression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DenoiseEngine {
    DeepFilterNet,
    FrameGate,
}

/// Result of denoising a WAV in memory. `level_change_db` is the change in RMS level (not a measured SNR), and is
/// 0.0 when either level is too low to compare.
#[derive(Debug, Clone)]
pub struct DenoiseOutcome {
    pub wav: Vec<u8>,
    pub engine: DenoiseEngine,
    pub original_rms: f32,
    pub enhanced_rms: f32,
    pub level_change_db: f32,
}

fn level_change_db(original: f32, enhanced: f32) -> f32 {
    if original > 0.0001 && enhanced > 0.0001 {
        20.0 * (original / enhanced).log10()
    } else {
        0.0
    }
}

pub struct NoiseSuppressor {
    model_path: PathBuf,
    is_model_loaded: bool,
    is_engine_ready: bool,
    attenuation_limit_db: f32,
}

impl NoiseSuppressor {
    /// DeepFilterNet3 bundle under `data/models/deepfilternet/`.
    /// Real noise suppression is the upstream `deep-filter` CLI, which works on whole files (`denoise_memory_wav`,
    /// `enhance_wav_file`). `process_frame` is NOT DeepFilterNet: it is a gentle gate that turns quiet samples
    /// down by `attenuation_limit_db`, used where only a live frame is available.
    pub fn new<P: AsRef<Path>>(model_dir: P) -> Result<Self> {
        let model_dir = model_dir.as_ref();
        let tar_path = model_tarball(model_dir);

        info!("Initializing DeepFilterNet3 adapter at {:?}", model_dir);

        let is_model_loaded = model_bundle_valid(model_dir);
        let cli_ready = resolve_cli_binary().is_some() && tar_path.is_file();
        let is_engine_ready = is_model_loaded && cli_ready;

        if !is_model_loaded {
            warn!(
                "DeepFilterNet3 weights missing. Run scripts/fetch_deepfilternet3_onnx.ps1 — see data/models/deepfilternet/README.md"
            );
        } else if !cli_ready {
            warn!(
                "Model weights present but deep-filter CLI missing. Run scripts/build_deepfilter_cli.ps1"
            );
        } else {
            info!("DeepFilterNet3 model + deep-filter CLI ready");
        }

        let model_path = tar_path;

        Ok(Self {
            model_path,
            is_model_loaded,
            is_engine_ready,
            attenuation_limit_db: DEFAULT_ATTENUATION_DB,
        })
    }

    pub fn process_frame(&self, input_frame: &[f32]) -> Vec<f32> {
        if input_frame.is_empty() {
            return Vec::new();
        }

        if self.is_model_loaded {
            self.apply_soft_suppression(input_frame)
        } else {
            self.apply_noise_gate(input_frame)
        }
    }

    fn apply_soft_suppression(&self, frame: &[f32]) -> Vec<f32> {
        let floor = 10f32.powf(-self.attenuation_limit_db / 20.0);
        frame
            .iter()
            .map(|&s| {
                let abs_s = s.abs();
                if abs_s < 0.015 {
                    s * floor
                } else {
                    s
                }
            })
            .collect()
    }

    fn apply_noise_gate(&self, frame: &[f32]) -> Vec<f32> {
        frame
            .iter()
            .map(|&s| if s.abs() < 0.01 { 0.0 } else { s })
            .collect()
    }

    pub fn enhance_wav_file(&self, input_wav: &Path, output_dir: &Path) -> Result<()> {
        if let Some(cli) = resolve_cli_binary() {
            let tar_path = crate::deepfilter_cli::model_tarball(&self.model_path);
            crate::deepfilter_cli::enhance_wav(&cli, &tar_path, input_wav, output_dir)
        } else {
            anyhow::bail!("DeepFilterNet CLI binary not ready. Run scripts/build_deepfilter_cli.ps1")
        }
    }

    /// Process raw in-memory WAV bytes with the DeepFilterNet CLI, or (only when it cannot run) the frame gate.
    /// The outcome says which one it was; nothing here is estimated or made up.
    pub fn denoise_memory_wav(&self, wav_bytes: &[u8]) -> Result<DenoiseOutcome> {
        if wav_bytes.is_empty() {
            return Ok(DenoiseOutcome { wav: Vec::new(), engine: DenoiseEngine::FrameGate, original_rms: 0.0, enhanced_rms: 0.0, level_change_db: 0.0 });
        }

        // 1. If CLI is ready, attempt CLI execution via temp files
        if self.is_engine_ready {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let scratch_dir = std::env::temp_dir().join(format!("aqua_df_denoise_{stamp}"));
            let input_wav = scratch_dir.join("input.wav");
            let out_dir = scratch_dir.join("out");

            if std::fs::create_dir_all(&scratch_dir).is_ok() && std::fs::write(&input_wav, wav_bytes).is_ok() {
                match self.enhance_wav_file(&input_wav, &out_dir) {
                    Ok(_) => {
                        let enhanced_path = crate::deepfilter_cli::enhanced_output_wav(&out_dir, &input_wav);
                        if enhanced_path.is_file() {
                            if let Ok(enhanced_bytes) = std::fs::read(&enhanced_path) {
                                let _ = std::fs::remove_dir_all(&scratch_dir);
                                let orig_rms = calculate_wav_bytes_rms(wav_bytes).unwrap_or(0.0);
                                let enh_rms = calculate_wav_bytes_rms(&enhanced_bytes).unwrap_or(0.0);
                                return Ok(DenoiseOutcome { wav: enhanced_bytes, engine: DenoiseEngine::DeepFilterNet, original_rms: orig_rms, enhanced_rms: enh_rms, level_change_db: level_change_db(orig_rms, enh_rms) });
                            }
                        }
                    }
                    Err(e) => {
                        warn!("DeepFilterNet CLI enhance_wav failed: {e}");
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&scratch_dir);
        }

        // 2. In-memory DSP / hound fallback
        use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
        use std::io::Cursor;

        let cursor = Cursor::new(wav_bytes);
        let mut reader = WavReader::new(cursor)?;
        let spec = reader.spec();

        let samples: Vec<f32> = match spec.sample_format {
            SampleFormat::Float => reader.samples::<f32>().filter_map(Result::ok).collect(),
            SampleFormat::Int => {
                // i64: for 32-bit audio `1 << 31` does not fit an i32 and flips every sample's sign
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .filter_map(Result::ok)
                    .map(|s| s as f32 / max_val)
                    .collect()
            }
        };

        let orig_len = samples.len().max(1) as f32;
        let orig_rms = (samples.iter().map(|&s| s * s).sum::<f32>() / orig_len).sqrt();

        let clean_samples = self.process_frame(&samples);
        let enh_rms = (clean_samples.iter().map(|&s| s * s).sum::<f32>() / orig_len).sqrt();

        let mut out_cursor = Cursor::new(Vec::new());
        let out_spec = WavSpec {
            channels: spec.channels,
            sample_rate: spec.sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };

        let mut writer = WavWriter::new(&mut out_cursor, out_spec)?;
        for &s in &clean_samples {
            let clamped = s.clamp(-1.0, 1.0);
            let int_sample = (clamped * 32767.0) as i16;
            writer.write_sample(int_sample)?;
        }
        writer.finalize()?;

        Ok(DenoiseOutcome { wav: out_cursor.into_inner(), engine: DenoiseEngine::FrameGate, original_rms: orig_rms, enhanced_rms: enh_rms, level_change_db: level_change_db(orig_rms, enh_rms) })
    }

    pub fn is_model_loaded(&self) -> bool {
        self.is_model_loaded
    }

    pub fn is_engine_ready(&self) -> bool {
        self.is_engine_ready
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn attenuation_limit_db(&self) -> f32 {
        self.attenuation_limit_db
    }
}

pub fn calculate_wav_bytes_rms(wav_bytes: &[u8]) -> Result<f32> {
    use hound::{SampleFormat, WavReader};
    use std::io::Cursor;

    let cursor = Cursor::new(wav_bytes);
    let mut reader = WavReader::new(cursor)?;
    let spec = reader.spec();

    let samples: Vec<f32> = match spec.sample_format {
        SampleFormat::Float => reader.samples::<f32>().filter_map(Result::ok).collect(),
        SampleFormat::Int => {
            // i64: for 32-bit audio `1 << 31` does not fit an i32 and flips every sample's sign
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .filter_map(Result::ok)
                .map(|s| s as f32 / max_val)
                .collect()
        }
    };

    if samples.is_empty() {
        return Ok(0.0);
    }

    let rms = (samples.iter().map(|&s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    Ok(rms)
}

