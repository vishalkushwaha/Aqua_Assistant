//! Audio Processing, Noise Cancellation, Biometrics, Whisper STT & Kokoro TTS Adapter

pub mod biometrics_ecapa;
pub mod cpu;
pub mod deepfilter_cli;
pub mod kokoro_cli;
pub mod whisper_cli;
pub mod mic_capture;
pub mod noise_cancellation;
pub mod silero_vad;
pub mod speech_gate;
pub mod stt_whisper;
pub mod tts_kokoro;
pub mod voice_standby;

pub use biometrics_ecapa::{extract_pcm16_samples, VoiceBiometricsEngine};
pub use mic_capture::MicCapture;
pub use noise_cancellation::NoiseSuppressor;
pub use silero_vad::SileroVadDetector;
pub mod engines;
pub mod process;
pub mod resample;
pub mod speaker_features;
pub use stt_whisper::WhisperSttEngine;
pub use tts_kokoro::KokoroTtsEngine;
pub use voice_standby::VoiceStandbyPipeline;
