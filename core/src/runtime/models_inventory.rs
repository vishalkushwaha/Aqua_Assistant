use serde::{Deserialize, Serialize};
use std::path::Path;

/// Which documented model artifacts exist under `data/models/`.
/// See `data/models/README.md` for official download sources.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelsInventory {
    pub deepfilternet_onnx: bool,
    pub whisper_ggml: bool,
    pub llm_gguf: bool,
    pub kokoro_onnx: bool,
    pub biometrics_onnx: bool,
    /// Laya HF cache present under `data/models/laya/models/`.
    #[serde(default)]
    pub laya_checkpoints: bool,
}

pub fn inspect_models(models_dir: &Path) -> ModelsInventory {
    let laya_models = models_dir.join("laya").join("models");
    ModelsInventory {
        deepfilternet_onnx: aqua_audio_adapter::deepfilter_cli::model_bundle_valid(
            &models_dir.join("deepfilternet"),
        ),
        whisper_ggml: aqua_audio_adapter::whisper_cli::model_bundle_valid(
            &models_dir.join("whisper"),
        ),
        llm_gguf: crate::modules::llm_router::llama_cli::model_bundle_valid(
            &models_dir.join("llm"),
        ),
        kokoro_onnx: aqua_audio_adapter::kokoro_cli::model_bundle_valid(
            &models_dir.join("kokoro"),
        ),
        biometrics_onnx: aqua_audio_adapter::biometrics_ecapa::MODEL_FILES
            .iter()
            .any(|f| models_dir.join("biometrics").join(f).is_file()),
        laya_checkpoints: laya_models.is_dir()
            && std::fs::read_dir(&laya_models)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false),
    }
}
