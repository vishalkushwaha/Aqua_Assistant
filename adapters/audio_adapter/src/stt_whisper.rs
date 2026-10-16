use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::whisper_cli::{model_bundle_valid, resolve_cli_binary, resolve_model_file, transcribe_file};

/// Below this level (about -60 dBFS) a frame holds no speech at all, so Whisper is not asked about it.
const SILENCE_RMS: f32 = 0.001;

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

fn pcm_to_wav_bytes(samples: &[f32]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec)?;
        for s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        w.finalize()?;
    }
    Ok(cursor.into_inner())
}

pub struct WhisperSttEngine {
    model_path: PathBuf,
    /// Where the model lives; readiness is checked against it each time it is asked, not once at start.
    model_dir: Option<PathBuf>,
    is_model_loaded: bool,
    is_engine_ready: bool,
    language: String,
    /// The model used for requests (after wake-up) when the person has chosen one; otherwise the default order applies.
    active_model: std::sync::RwLock<Option<String>>,
}

impl WhisperSttEngine {
    /// Whisper.cpp STT — official GGUF under `data/models/whisper/` (see `data/models/whisper/README.md`).
    pub fn new<P: AsRef<Path>>(model_dir: P) -> Result<Self> {
        let model_dir = model_dir.as_ref();
        let model_path = resolve_model_file(model_dir).unwrap_or_else(|| {
            model_dir.join("ggml-medium.en.bin")
        });

        info!("Initializing Whisper.cpp STT at {:?}", model_path);

        let is_model_loaded = model_bundle_valid(model_dir);
        let is_engine_ready = is_model_loaded && resolve_cli_binary().is_some();

        if !is_model_loaded {
            warn!(
                "Whisper GGUF missing. Run scripts/fetch_whisper_medium_en.ps1 — see data/models/whisper/README.md"
            );
        } else if !is_engine_ready {
            warn!("Whisper model present but whisper-cli not built. Run scripts/build_whisper_cli.ps1");
        } else {
            info!("Whisper.cpp model + whisper-cli ready");
        }

        Ok(Self {
            model_path,
            model_dir: Some(model_dir.to_path_buf()),
            is_model_loaded,
            is_engine_ready,
            language: "en".to_string(),
            active_model: std::sync::RwLock::new(None),
        })
    }

    /// Transcribe 16 kHz mono PCM with whisper-cli. Never invents text: if speech-to-text is not available this
    /// is an error, and silence or noise gives an empty transcript (without running Whisper at all when the audio
    /// is digital silence).
    pub fn transcribe(&self, pcm_16k_samples: &[f32]) -> Result<String> {
        if pcm_16k_samples.is_empty() || rms(pcm_16k_samples) < SILENCE_RMS {
            return Ok(String::new());
        }
        if !self.is_engine_ready() {
            anyhow::bail!("speech-to-text is not available: {}", self.unavailable_reason());
        }
        let wav = pcm_to_wav_bytes(pcm_16k_samples)?;
        self.transcribe_memory_wav(&wav)
    }

    /// Choose the model used for requests (after wake-up); None goes back to the default order.
    pub fn set_active_model(&self, name: Option<String>) {
        *self.active_model.write().unwrap_or_else(|p| p.into_inner()) = name.filter(|n| !n.trim().is_empty());
    }

    pub fn active_model_name(&self) -> Option<String> {
        self.active_model.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The Whisper models in the models folder (file name and size in bytes), smallest first. The voice-activity model and
    /// anything that is not a speech model are left out.
    pub fn available_models(&self) -> Vec<(String, u64)> {
        let Some(dir) = &self.model_dir else { return Vec::new() };
        let mut v: Vec<(String, u64)> = std::fs::read_dir(dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        (name.starts_with("ggml-") && name.ends_with(".bin") && !name.starts_with("ggml-silero")).then(|| (name, e.metadata().map(|m| m.len()).unwrap_or(0)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|(_, size)| *size);
        v
    }

    pub fn model_dir(&self) -> Option<&Path> {
        self.model_dir.as_deref()
    }

    /// A speech model file name: `ggml-*.bin`, not the Silero voice-activity file, and not a path.
    fn speech_model_name(name: &str) -> bool {
        !name.is_empty() && !name.contains("..") && !name.contains('/') && !name.contains('\\') && name.starts_with("ggml-") && name.ends_with(".bin") && !name.starts_with("ggml-silero")
    }

    /// Copy a Whisper `.bin` into the models folder.
    pub fn import_model(&self, source: &Path) -> Result<String> {
        let dir = self.model_dir.as_ref().ok_or_else(|| anyhow::anyhow!("Whisper models folder is not set"))?;
        if !source.is_file() {
            anyhow::bail!("not a file: {}", source.display());
        }
        let name = source.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !Self::speech_model_name(name) {
            anyhow::bail!("the file must be a Whisper model named ggml-*.bin");
        }
        std::fs::create_dir_all(dir)?;
        let dest = dir.join(name);
        if source != dest.as_path() {
            std::fs::copy(source, &dest).map_err(|e| anyhow::anyhow!("could not copy into {}: {e}", dest.display()))?;
        }
        Ok(name.to_string())
    }

    /// Delete one Whisper model by file name.
    pub fn remove_model(&self, filename: &str) -> Result<()> {
        let dir = self.model_dir.as_ref().ok_or_else(|| anyhow::anyhow!("Whisper models folder is not set"))?;
        if !Self::speech_model_name(filename) {
            anyhow::bail!("not a Whisper model name: {filename}");
        }
        let path = dir.join(filename);
        if !path.is_file() {
            anyhow::bail!("Whisper model not found: {filename}");
        }
        std::fs::remove_file(&path).map_err(|e| anyhow::anyhow!("could not delete {}: {e}", path.display()))?;
        if self.active_model_name().as_deref() == Some(filename) {
            self.set_active_model(None);
        }
        Ok(())
    }

    /// Whether the voice-activity model is in use (Whisper then only transcribes what it judges to be speech).
    pub fn vad_active(&self) -> bool {
        crate::whisper_cli::vad_model_for(&self.model_for(None)).is_some()
    }

    /// Why `transcribe` cannot run, in words a person can act on.
    pub fn unavailable_reason(&self) -> &'static str {
        if !self.is_model_loaded() {
            "the Whisper model is missing (see data/models/whisper/README.md)"
        } else {
            "whisper-cli is not built (run scripts/build_whisper_cli.ps1)"
        }
    }

    pub fn transcribe_wav_file<P: AsRef<Path>>(&self, wav_path: P) -> Result<String> {
        self.transcribe_wav_file_with_model(wav_path, None)
    }

    pub fn transcribe_wav_file_with_model<P: AsRef<Path>>(&self, wav_path: P, model_override: Option<&str>) -> Result<String> {
        let cli = resolve_cli_binary()
            .ok_or_else(|| anyhow::anyhow!("whisper-cli binary not found"))?;
        let target_model = self.model_for(model_override);

        transcribe_file(&cli, &target_model, wav_path.as_ref())
    }

    /// Transcribe in-memory WAV bytes directly without disk I/O.
    pub fn transcribe_memory_wav(&self, wav_bytes: &[u8]) -> Result<String> {
        self.transcribe_memory_wav_with_model(wav_bytes, None)
    }

    pub fn transcribe_memory_wav_with_model(&self, wav_bytes: &[u8], model_override: Option<&str>) -> Result<String> {
        let cli = resolve_cli_binary()
            .ok_or_else(|| anyhow::anyhow!("whisper-cli binary not found"))?;
        let target_model = self.model_for(model_override);

        crate::whisper_cli::transcribe_wav_bytes(&cli, &target_model, wav_bytes)
    }

    /// A dictation clip: primed with the person's own terms (names, products, jargon) so that they are written as the person writes them; with none, the assistant's name.
    pub fn transcribe_memory_wav_with_prompt(&self, wav_bytes: &[u8], model_override: Option<&str>, prompt: &str) -> Result<String> {
        let cli = resolve_cli_binary().ok_or_else(|| anyhow::anyhow!("whisper-cli binary not found"))?;
        let target_model = self.model_for(model_override);

        crate::whisper_cli::transcribe_wav_bytes_prompted(&cli, &target_model, wav_bytes, crate::whisper_cli::Lane::Request, prompt)
    }

    /// The same for the listeners that run all day (the wake word, the spoken "stop"): few threads and a low priority, so that
    /// they never slow down the model that is answering.
    pub fn transcribe_memory_wav_listening(&self, wav_bytes: &[u8], model_override: Option<&str>) -> Result<String> {
        let cli = resolve_cli_binary()
            .ok_or_else(|| anyhow::anyhow!("whisper-cli binary not found"))?;
        let target_model = self.model_for(model_override);

        crate::whisper_cli::transcribe_wav_bytes_in(&cli, &target_model, wav_bytes, crate::whisper_cli::Lane::Listening)
    }

    /// The model file a request uses, looked up now: a model downloaded (or replaced) after start is found, and one named
    /// by `model_override` is used if it exists. A name that does not exist falls back to the default model, so the
    /// caller can compare this with what it asked for and say which one really ran.
    pub fn model_for(&self, model_override: Option<&str>) -> PathBuf {
        let mut default = match &self.model_dir {
            Some(dir) => resolve_model_file(dir).unwrap_or_else(|| self.model_path.clone()),
            None => self.model_path.clone(),
        };
        // What the person chose for requests, if that file is there.
        if model_override.is_none() {
            if let Some(chosen) = self.active_model_name().filter(|n| Path::new(n).file_name().and_then(|f| f.to_str()) == Some(n.as_str())) {
                let p = default.parent().unwrap_or(Path::new(".")).join(&chosen);
                if p.is_file() {
                    default = p;
                }
            }
        }
        if let Some(name) = model_override {
            // A bare file name only: a path in the name must not reach outside the models folder.
            if Path::new(name).file_name().and_then(|n| n.to_str()) == Some(name) {
                let p = default.parent().unwrap_or(Path::new(".")).join(name);
                if p.is_file() {
                    return p;
                }
            }
        }
        default
    }

    /// Is a Whisper model on disk right now? (Checked live: a model downloaded or deleted after start counts.)
    pub fn is_model_loaded(&self) -> bool {
        match &self.model_dir {
            Some(dir) => model_bundle_valid(dir),
            None => self.is_model_loaded,
        }
    }

    /// Can speech be transcribed right now: the model is there and `whisper-cli` can be found.
    pub fn is_engine_ready(&self) -> bool {
        match &self.model_dir {
            Some(_) => self.is_model_loaded() && resolve_cli_binary().is_some(),
            None => self.is_engine_ready,
        }
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn language(&self) -> &str {
        &self.language
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_without_whisper() -> WhisperSttEngine {
        WhisperSttEngine { model_path: PathBuf::from("none.bin"), model_dir: None, is_model_loaded: false, is_engine_ready: false, language: "en".into(), active_model: std::sync::RwLock::new(None) }
    }

    #[test]
    fn silence_is_an_empty_transcript_and_never_invented_text() {
        let e = engine_without_whisper();
        assert_eq!(e.transcribe(&[]).unwrap(), "");
        assert_eq!(e.transcribe(&vec![0.0; 16_000]).unwrap(), "");
    }

    #[test]
    fn real_audio_without_a_working_engine_is_an_error_not_a_made_up_sentence() {
        let e = engine_without_whisper();
        let err = e.transcribe(&vec![0.2; 16_000]).unwrap_err().to_string();
        assert!(err.contains("speech-to-text is not available") && err.contains("model is missing"), "{err}");
        let mut no_cli = engine_without_whisper();
        no_cli.is_model_loaded = true;
        assert!(no_cli.transcribe(&vec![0.2; 16_000]).unwrap_err().to_string().contains("whisper-cli is not built"));
    }

    #[test]
    fn pcm_becomes_a_valid_16k_mono_wav() {
        let wav = pcm_to_wav_bytes(&[0.0, 0.5, -0.5, 2.0]).unwrap();
        let r = hound::WavReader::new(std::io::Cursor::new(wav)).unwrap();
        assert_eq!((r.spec().channels, r.spec().sample_rate, r.spec().bits_per_sample), (1, 16_000, 16));
        assert_eq!(r.into_samples::<i16>().map(|s| s.unwrap()).collect::<Vec<_>>(), vec![0, 16383, -16383, 32767]);
    }
}
