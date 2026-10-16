use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::kokoro_cli::{
    cli_available, model_bundle_valid, resolve_cli, resolve_model_file, resolve_voices_file,
    synthesize_to_wav, wav_bytes_to_f32_mono, MODEL_FILENAME,
};

pub struct KokoroTtsEngine {
    model_dir: PathBuf,
    model_path: PathBuf,
    voices_path: PathBuf,
    sample_rate: u32,
    default_voice: String,
    default_lang: String,
}

impl KokoroTtsEngine {
    /// Kokoro TTS — official ONNX + voices under `data/models/kokoro/` (see README there).
    pub fn new<P: AsRef<Path>>(model_dir: P) -> Result<Self> {
        let model_dir = model_dir.as_ref().to_path_buf();
        let model_path = resolve_model_file(&model_dir)
            .unwrap_or_else(|| model_dir.join(MODEL_FILENAME));
        let voices_path = resolve_voices_file(&model_dir)
            .unwrap_or_else(|| model_dir.join(crate::kokoro_cli::VOICES_FILENAME));

        info!(
            "Initializing Kokoro TTS from model {:?} voices {:?}",
            model_path, voices_path
        );

        let is_model_loaded = model_bundle_valid(&model_dir);
        let is_engine_ready = is_model_loaded && cli_available();

        if !is_model_loaded {
            warn!(
                "Kokoro weights missing. Run scripts/fetch_kokoro_v1.ps1 — see data/models/kokoro/README.md"
            );
        } else if !is_engine_ready {
            warn!("Kokoro models present but CLI missing. Run scripts/setup_kokoro_cli.ps1");
        } else {
            info!("Kokoro ONNX + voices + kokoro-tts CLI ready");
        }

        Ok(Self {
            model_dir,
            model_path,
            voices_path,
            sample_rate: 24000,
            default_voice: "af_sarah".to_string(),
            default_lang: "en-us".to_string(),
        })
    }

    /// Synthesize text to mono PCM floats (pipeline stage).
    pub fn synthesize(&self, text: &str) -> Result<Vec<f32>> {
        self.synthesize_with_voice(text, &self.default_voice, &self.default_lang)
    }

    pub fn synthesize_with_voice(&self, text: &str, voice: &str, lang: &str) -> Result<Vec<f32>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }

        if let Some((cli, model, voices)) = self.ready() {
            let wav = synthesize_to_wav(
                &cli,
                &model,
                &voices,
                text.trim(),
                voice,
                lang,
            )?;
            let (samples, sr) = wav_bytes_to_f32_mono(&wav)?;
            info!("Kokoro synthesized {} samples at {} Hz", samples.len(), sr);
            return Ok(samples);
        }

        // Silent fallback buffer (~0.5 s at default rate)
        Ok(vec![0.0f32; (self.sample_rate / 2) as usize])
    }

    /// Full WAV file bytes for HTTP/API consumers.
    pub fn synthesize_wav(&self, text: &str, voice: &str, lang: &str) -> Result<Vec<u8>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }

        if let Some((cli, model, voices)) = self.ready() {
            return synthesize_to_wav(
                &cli,
                &model,
                &voices,
                text.trim(),
                voice,
                lang,
            );
        }

        Ok(Vec::new())
    }

    /// Is the Kokoro model bundle on disk right now? (Checked live, not once at start.)
    pub fn is_model_loaded(&self) -> bool {
        model_bundle_valid(&self.model_dir)
    }

    /// Can speech be produced right now: the bundle is there and the Kokoro command can be found.
    pub fn is_engine_ready(&self) -> bool {
        self.ready().is_some()
    }

    /// The command and the model files as they are right now (a bundle downloaded after start may be named differently).
    fn ready(&self) -> Option<(crate::kokoro_cli::KokoroCli, PathBuf, PathBuf)> {
        let model = crate::kokoro_cli::resolve_model_file(&self.model_dir)?;
        let voices = crate::kokoro_cli::resolve_voices_file(&self.model_dir)?;
        Some((resolve_cli()?, model, voices))
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn voices_path(&self) -> &Path {
        &self.voices_path
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }
}
