//! The five engines in one pipeline object, and the health of its stages (move only: this was part of `pipeline.rs`).

use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct StageStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub model_present: bool,
    pub engine_ready: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PipelineStageHealth {
    pub stages: Vec<StageStatus>,
    /// All four engines ready (full voice frame path with live CLIs).
    pub pipeline_ready: bool,
    /// LLM + TTS available (typed text → speech path).
    pub text_pipeline_ready: bool,
}

pub struct AquaEndToEndPipeline {
    suppressor: NoiseSuppressor,
    stt_engine: WhisperSttEngine,
    llm_router: LlamaRouterEngine,
    tts_engine: KokoroTtsEngine,
    bio_engine: std::sync::Mutex<VoiceBiometricsEngine>,
}

impl AquaEndToEndPipeline {
    /// Initialises the complete end-to-end local CPU pipeline.
    pub fn new<P: AsRef<Path>>(models_base_dir: P) -> Result<Self> {
        let base_dir = models_base_dir.as_ref();
        info!(
            "Initializing Aqua End-to-End Pipeline from models base dir: {:?}",
            base_dir
        );

        let df_dir = base_dir.join("deepfilternet");
        let whisper_dir = base_dir.join("whisper");
        let llm_dir = base_dir.join("llm");
        let kokoro_dir = base_dir.join("kokoro");
        let bio_dir = base_dir.join("biometrics");

        let suppressor = NoiseSuppressor::new(df_dir)?;
        let stt_engine = WhisperSttEngine::new(whisper_dir)?;
        let llm_router = LlamaRouterEngine::new(llm_dir)?;
        let tts_engine = KokoroTtsEngine::new(kokoro_dir)?;
        let bio_engine = VoiceBiometricsEngine::new(&bio_dir)?;

        info!("All 5 AI engines initialized in End-to-End Pipeline.");
        Ok(Self {
            suppressor,
            stt_engine,
            llm_router,
            tts_engine,
            bio_engine: std::sync::Mutex::new(bio_engine),
        })
    }

    pub fn stage_health(&self) -> PipelineStageHealth {
        collect_stage_health(
            &self.suppressor,
            &self.stt_engine,
            &self.llm_router,
            &self.tts_engine,
        )
    }

    /// Typed input path: Llama → Kokoro (skips mic / DF / Whisper).
    pub fn process_text_to_speech(
        &self,
        text: &str,
        voice: &str,
        lang: &str,
    ) -> Result<PipelineTextResult> {
        run_text_to_speech(&self.llm_router, &self.tts_engine, text, voice, lang)
    }

    /// WAV on disk: optional DeepFilterNet CLI → Whisper CLI → Llama → Kokoro.
    pub fn process_wav_file_to_speech<P: AsRef<Path>>(
        &self,
        deepfilter_model_dir: P,
        input_wav: P,
        voice: &str,
        lang: &str,
    ) -> Result<PipelineWavResult> {
        let mut bio = self.bio_engine.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        run_wav_file_to_speech(
            &self.suppressor,
            &self.stt_engine,
            &self.llm_router,
            &self.tts_engine,
            &mut bio,
            false, // security_enabled default false for tests
            "Hey Aqua", // wakeup_word default
            false, // active_session default false
            deepfilter_model_dir.as_ref(),
            input_wav.as_ref(),
            voice,
            lang,
        )
    }

    /// Full frame path: DeepFilterNet → Whisper (PCM stub or CLI) → Llama → Kokoro PCM.
    pub fn process_audio_to_speech(&self, noisy_audio_frame: &[f32]) -> Result<(String, String, Vec<f32>)> {
        let mut bio = self.bio_engine.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        run_audio_frame_to_speech(
            &self.suppressor,
            &self.stt_engine,
            &self.llm_router,
            &self.tts_engine,
            &mut bio,
            false, // security_enabled
            "Hey Aqua", // wakeup_word
            false, // active_session
            noisy_audio_frame,
        )
    }

    pub fn is_all_models_ready(&self) -> bool {
        self.stage_health().pipeline_ready
    }

    pub fn suppressor(&self) -> &NoiseSuppressor {
        &self.suppressor
    }

    pub fn stt_engine(&self) -> &WhisperSttEngine {
        &self.stt_engine
    }

    pub fn llm_router(&self) -> &LlamaRouterEngine {
        &self.llm_router
    }

    pub fn tts_engine(&self) -> &KokoroTtsEngine {
        &self.tts_engine
    }
}

pub fn collect_stage_health(
    suppressor: &NoiseSuppressor,
    stt: &WhisperSttEngine,
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
) -> PipelineStageHealth {
    let stages = vec![
        StageStatus {
            id: "deepfilternet",
            label: "DeepFilterNet3",
            model_present: suppressor.is_model_loaded(),
            engine_ready: suppressor.is_engine_ready(),
        },
        StageStatus {
            id: "whisper",
            label: "Whisper.cpp STT",
            model_present: stt.is_model_loaded(),
            engine_ready: stt.is_engine_ready(),
        },
        StageStatus {
            id: "llama",
            label: "Llama.cpp LLM",
            model_present: llm.is_model_loaded(),
            engine_ready: llm.is_engine_ready(),
        },
        StageStatus {
            id: "kokoro",
            label: "Kokoro TTS",
            model_present: tts.is_model_loaded(),
            engine_ready: tts.is_engine_ready(),
        },
    ];

    let pipeline_ready = stages.iter().all(|s| s.engine_ready);
    let text_pipeline_ready = llm.is_model_loaded() && tts.is_model_loaded();

    PipelineStageHealth {
        stages,
        pipeline_ready,
        text_pipeline_ready,
    }
}
