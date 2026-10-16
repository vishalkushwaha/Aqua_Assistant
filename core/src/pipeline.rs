//! The voice and text turn, from the audio (or the typed words) to the spoken answer: the speech gate, noise removal, Whisper, the voice lock, routing, the agent, Kokoro (the page is `documentation/VOICE_PIPELINE.md`).
//!
//! The code is cut by what it does (move only: no line of code was changed, and every `crate::pipeline::X` is where it was because this file re-exports each part):
//! `speech_checks` (is there speech in the audio, was it Whisper's invention, the trimmed copy), `turn_results` (what a turn returns and tells its listener), `engines` (the five engines in one object and their health),
//! `speaker` (the answer made into speech, piece by piece), `assistant_reply` (what a turn answers: the programs that need no model, the gate, then chat or the agent), `chat_answer` (the chat prompt and its reply),
//! `agent_turn` (the tool agent's run, the question it leaves, the run that goes on), `text_turn` (a typed turn) and `voice_turn` (a frame or a recording through noise removal, Whisper, the wake and voice checks, the answer and its speech).
//! This file keeps the imports that every part reaches through `use super::*` and the declarations and re-exports. The tests are the `*_tests` modules, each in a file of its own.

use aqua_audio_adapter::deepfilter_cli::{
    enhance_wav, enhanced_output_wav, model_tarball, resolve_cli_binary,
};
use aqua_audio_adapter::noise_cancellation::NoiseSuppressor;
use aqua_audio_adapter::stt_whisper::WhisperSttEngine;
use aqua_audio_adapter::tts_kokoro::KokoroTtsEngine;
use aqua_audio_adapter::VoiceBiometricsEngine;
use aqua_mcp_adapters::browser::BrowserBackend;
use aqua_mcp_adapters::StdioMcpClient;
use crate::laya_validator::{GateDecision, GateRoute, LayaGate};
use crate::modules::browser_agent::{run_mcp_agent_shared, with_windows_caller};
use aqua_mcp_adapters::WindowsCaller;
use crate::modules::llm_router::{strip_reasoning_tags, LlamaRouterEngine};
use crate::voice_commands::{
    passes_wake_gate, resolve_session_command, session_action_label, CommandHandling,
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Is there speech in the audio, was it Whisper's invention, and the trimmed copy of a recording.
mod speech_checks;
pub use self::speech_checks::*;

/// What a turn returns, what a streamed turn tells its listener, and the note for a refused turn.
mod turn_results;
pub use self::turn_results::*;

/// The five engines in one pipeline object, and the health of its stages.
mod engines;
pub use self::engines::*;

/// The answer made into speech, one piece at a time.
mod speaker;
use self::speaker::*;

/// What a turn answers: the programs without a model, the safety gate, then chat or the agent.
mod assistant_reply;
use self::assistant_reply::*;

/// The chat prompt and the plain chat reply.
mod chat_answer;
pub use self::chat_answer::*;

/// One run of the tool agent for a turn, the question it leaves, and the run that goes on.
mod agent_turn;
pub use self::agent_turn::*;

/// A typed turn.
mod text_turn;
pub use self::text_turn::*;

/// A spoken turn: a frame of audio or a recording to a spoken answer.
mod voice_turn;
pub use self::voice_turn::*;

#[cfg(test)]
mod speech_checks_tests;

#[cfg(test)]
mod turn_results_tests;

#[cfg(test)]
mod assistant_reply_tests;

#[cfg(test)]
mod agent_turn_tests;

#[cfg(test)]
mod chat_answer_tests;

#[cfg(test)]
mod streaming_tests;
