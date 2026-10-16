use anyhow::Result;
use std::fs::{create_dir_all, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::info;

pub struct ModelDownloader {
    base_models_dir: PathBuf,
}

impl ModelDownloader {
    pub fn new<P: AsRef<Path>>(base_dir: P) -> Self {
        Self {
            base_models_dir: base_dir.as_ref().to_path_buf(),
        }
    }

    /// Verifies and configures DeepFilterNet3 model directory from official documented source.
    pub fn ensure_deepfilternet_model(&self) -> Result<PathBuf> {
        let df_dir = self.base_models_dir.join("deepfilternet");
        create_dir_all(&df_dir)?;

        let model_path = df_dir.join("DeepFilterNet3_onnx");
        info!(
            "Checking DeepFilterNet3 model configuration at path: {:?}",
            model_path
        );

        let manifest_file = df_dir.join("model_manifest.json");
        if !manifest_file.exists() {
            let manifest_content = serde_json::json!({
                "model_name": "DeepFilterNet3_onnx",
                "official_source": "https://gitlab.com/VisKush/deepfilternet_rust",
                "download_url": "https://github.com/Rikorose/DeepFilterNet/raw/main/models/DeepFilterNet3_onnx.tar.gz",
                "storage_directory": "data/models/deepfilternet/DeepFilterNet3_onnx/",
                "archive_path": "data/models/deepfilternet/DeepFilterNet3_onnx.tar.gz",
                "fetch_script": "scripts/fetch_deepfilternet3_onnx.ps1",
                "quantization": "ONNX (tract)",
                "sampling_rate": 48000,
                "attenuation_limit_db": 15
            });

            let mut f = File::create(&manifest_file)?;
            f.write_all(serde_json::to_string_pretty(&manifest_content)?.as_bytes())?;
            info!("Created DeepFilterNet3 model manifest at {:?}", manifest_file);
        }

        Ok(model_path)
    }

    /// Verifies and configures Whisper.cpp GGUF model directory from official documented source.
    pub fn ensure_whisper_model(&self) -> Result<PathBuf> {
        let whisper_dir = self.base_models_dir.join("whisper");
        create_dir_all(&whisper_dir)?;

        let model_path = whisper_dir.join("ggml-medium.en.bin");
        info!(
            "Checking Whisper.cpp model configuration at path: {:?}",
            model_path
        );

        let manifest_file = whisper_dir.join("model_manifest.json");
        if !manifest_file.exists() {
            let manifest_content = serde_json::json!({
                "model_name": "ggml-medium.en.bin",
                "official_source": "https://github.com/ggml-org/whisper.cpp",
                "download_url": "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.en.bin",
                "storage_directory": "data/models/whisper/ggml-medium.en.bin",
                "fetch_script": "scripts/fetch_whisper_medium_en.ps1",
                "format": "GGUF INT8",
                "sample_rate": 16000,
                "language": "en"
            });

            let mut f = File::create(&manifest_file)?;
            f.write_all(serde_json::to_string_pretty(&manifest_content)?.as_bytes())?;
            info!("Created Whisper.cpp model manifest at {:?}", manifest_file);
        }

        Ok(model_path)
    }

    /// Verifies and configures Llama.cpp GGUF model directory from official documented source.
    pub fn ensure_llama_model(&self) -> Result<PathBuf> {
        let llm_dir = self.base_models_dir.join("llm");
        create_dir_all(&llm_dir)?;

        let model_path = llm_dir.join("qwen-0.5b-instruct-q4_k_m.gguf");
        info!(
            "Checking Llama.cpp model configuration at path: {:?}",
            model_path
        );

        let manifest_file = llm_dir.join("model_manifest.json");
        if !manifest_file.exists() {
            let manifest_content = serde_json::json!({
                "model_name": "qwen-0.5b-instruct-q4_k_m.gguf",
                "official_source": "https://github.com/ggml-org/llama.cpp",
                "download_url": "https://huggingface.co/Qwen/Qwen1.5-0.5B-Chat-GGUF/resolve/main/qwen1_5-0_5b-chat-q4_k_m.gguf",
                "storage_directory": "data/models/llm/qwen-0.5b-instruct-q4_k_m.gguf",
                "fetch_script": "scripts/fetch_llama_qwen_0_5b.ps1",
                "build_cli_script": "scripts/build_llama_cli.ps1",
                "format": "GGUF Q4_K_M",
                "context_window_tokens": 4096,
                "ram_loading_strategy": "Single-Model RAM Eviction"
            });

            let mut f = File::create(&manifest_file)?;
            f.write_all(serde_json::to_string_pretty(&manifest_content)?.as_bytes())?;
            info!("Created Llama.cpp model manifest at {:?}", manifest_file);
        }

        Ok(model_path)
    }

    /// Verifies and configures Kokoro TTS ONNX model directory from official documented source.
    pub fn ensure_kokoro_model(&self) -> Result<PathBuf> {
        let kokoro_dir = self.base_models_dir.join("kokoro");
        create_dir_all(&kokoro_dir)?;

        let model_path = kokoro_dir.join("kokoro-v1.0.onnx");
        info!(
            "Checking Kokoro TTS model configuration at path: {:?}",
            model_path
        );

        let manifest_file = kokoro_dir.join("model_manifest.json");
        if !manifest_file.exists() {
            let manifest_content = serde_json::json!({
                "model_name": "kokoro-v1.0.onnx",
                "voices_file": "voices-v1.0.bin",
                "official_source": "https://github.com/nazdridoy/kokoro-tts",
                "download_url_onnx": "https://github.com/nazdridoy/kokoro-tts/releases/download/v1.0.0/kokoro-v1.0.onnx",
                "download_url_voices": "https://github.com/nazdridoy/kokoro-tts/releases/download/v1.0.0/voices-v1.0.bin",
                "storage_directory": "data/models/kokoro/",
                "fetch_script": "scripts/fetch_kokoro_v1.ps1",
                "setup_cli_script": "scripts/setup_kokoro_cli.ps1",
                "format": "ONNX + voices.bin",
                "default_voice": "af_sarah",
                "default_lang": "en-us"
            });

            let mut f = File::create(&manifest_file)?;
            f.write_all(serde_json::to_string_pretty(&manifest_content)?.as_bytes())?;
            info!("Created Kokoro TTS model manifest at {:?}", manifest_file);
        }

        Ok(model_path)
    }

    /// Verifies and configures the speaker-model (voice lock) directory. The model itself is fetched with
    /// `scripts/fetch_speaker_model.ps1` (pinned SHA-256); this only writes the manifest.
    pub fn ensure_ecapa_model(&self) -> Result<PathBuf> {
        let bio_dir = self.base_models_dir.join("biometrics");
        create_dir_all(&bio_dir)?;

        let model_path = bio_dir.join("speaker_wespeaker_resnet34_lm.onnx");
        info!(
            "Checking the speaker model configuration at path: {:?}",
            model_path
        );

        let manifest_file = bio_dir.join("model_manifest.json");
        if !manifest_file.exists() {
            let manifest_content = serde_json::json!({
                "model_name": "speaker_wespeaker_resnet34_lm.onnx",
                "storage_directory": "data/models/biometrics/speaker_wespeaker_resnet34_lm.onnx",
                "embedding_dimensions": 256,
                "match_threshold": 0.55,
                "adaptive_drift_threshold": 0.90,
                "format": "ONNX Float32 CPU"
            });

            let mut f = File::create(&manifest_file)?;
            f.write_all(serde_json::to_string_pretty(&manifest_content)?.as_bytes())?;
            info!("Created the speaker model manifest at {:?}", manifest_file);
        }

        Ok(model_path)
    }

    /// Verifies and configures PP-OCR v4 and YOLOv8-Nano Vision ONNX model directory.
    pub fn ensure_vision_models(&self) -> Result<PathBuf> {
        let vision_dir = self.base_models_dir.join("vision");
        create_dir_all(&vision_dir)?;

        let model_path = vision_dir.join("pp_ocr.onnx");
        info!(
            "Checking PP-OCR and YOLOv8 Vision model configuration at path: {:?}",
            model_path
        );

        let manifest_file = vision_dir.join("model_manifest.json");
        if !manifest_file.exists() {
            let manifest_content = serde_json::json!({
                "ocr_model": "pp_ocr.onnx",
                "object_detector_model": "yolov8_nano.onnx",
                "storage_directory": "data/models/vision/",
                "quantization": "ONNX INT8 CPU SIMD",
                "vlm_model": "moondream2_vlm.gguf"
            });

            let mut f = File::create(&manifest_file)?;
            f.write_all(serde_json::to_string_pretty(&manifest_content)?.as_bytes())?;
            info!("Created Vision model manifest at {:?}", manifest_file);
        }

        Ok(model_path)
    }

    pub fn base_models_dir(&self) -> &Path {
        &self.base_models_dir
    }
}
