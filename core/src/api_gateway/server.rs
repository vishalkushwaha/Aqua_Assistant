//! The HTTP gateway of the core: the state every handler is given, what is put around a request, the routes, and the start of the server. Card OA3-06 cut the file, which had
//! about 6,200 lines, into the files of `server/` by what each does; the code was moved, and only what the move forced was written new (a visibility, the depth of a `super::` path, a `routes()` for each family).
//!
//! This file is the root of the module. It holds the imports that every part reaches through `use super::*`, declares the parts and re-exports what they define (so that every
//! `crate::api_gateway::server::X` used before is still there), and declares the tests, one file for each module, at the end. A family of routes is one file with a `routes()` that holds
//! its `.route(...)` lines next to its handlers (the OpenAPI generator reads the routes and the handlers of one file, see `FILES` in `openapi.rs`); `router.rs` merges them.

use anyhow::Result;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

use aqua_audio_adapter::{extract_pcm16_samples, KokoroTtsEngine, NoiseSuppressor, VoiceBiometricsEngine, WhisperSttEngine};
use aqua_mcp_adapters::{
    ensure_browser_mcp_ready, run_browser_mcp_setup_script, aqua_workspace_root,
    EnsureBrowserMcpOptions, WindowsMcpClient,
};
use aqua_mcp_adapters::browser::{
    BackendKind, BrowserBackend, BrowserPolicyConfig, Decision,
};
use crate::laya_validator::{GateRoute, LayaGate};
use crate::modules::browser_agent::{
    phase_event, run_mcp_agent_shared, run_mcp_agent_shared_with_progress, AgentStep,
    McpAgentResult,
};
use crate::modules::llm_router::LlamaRouterEngine;
use crate::pipeline::{
    collect_stage_health, run_audio_frame_to_speech_with_mcp, run_wav_file_to_speech_streaming, run_wav_file_to_speech_with_mcp,
    PipelineStageHealth,
};
use crate::runtime::{inspect_models, ModelsInventory};
use crate::startup_health::{ManagedServices, StartupHealthReport};
use crate::storage::DatabaseManager;
use std::sync::{Mutex, PoisonError};

use super::auth::{load_or_create_token, load_or_create_ui_token, require_local_auth, token_path, ui_scope_required, ApiAuth, Scope};
use super::stream_events::StreamEvent;
pub use super::contract::api_error_code;

// What every handler shares: the state, the layers and limits of a request, the one turn at a time, the warm-up, and the answers that say a request failed.
mod state;
pub use self::state::*;
mod middleware;
pub(crate) use self::middleware::*;
mod turn;
pub(in crate::api_gateway) use self::turn::*;
mod warmup;
pub use self::warmup::*;
mod errors;
pub use self::errors::*;

// The routes, by family. The status of the core and the gate, the file index, the WebSocket, and the consents and questions that wait for an answer:
mod status_api;
pub use self::status_api::*;
mod index_api;
pub use self::index_api::*;
mod ws;
mod approvals_api;
pub use self::approvals_api::*;

// The turn: answered in one piece, streamed, and what is said to a turn that runs or stopped:
mod pipeline_api;
pub use self::pipeline_api::*;
mod stream_api;
// Only the tests name what `stream_api` shares (`streamed_done`, `stream_turn_with`): in a build without them nothing would come through this line.
#[cfg(test)]
use self::stream_api::*;
mod turn_control_api;
pub use self::turn_control_api::*;

// The person's voice and the engines: listening while idle, speech and sound, the voice settings, and the local language model:
mod listen_api;
pub use self::listen_api::*;
mod speech_api;
pub use self::speech_api::*;
mod voice_settings_api;
pub use self::voice_settings_api::*;
mod llm_api;
pub use self::llm_api::*;

// What reaches the computer: Browser MCP and Windows-MCP, the tool host and the browser layer:
mod mcp_api;
pub use self::mcp_api::*;
mod tools_api;
pub use self::tools_api::*;
mod browser_api;
pub use self::browser_api::*;

// The agent request, and its stream of events:
mod agent_api;
pub use self::agent_api::*;
mod agent_stream_api;

// The start of the gateway, the layers over the routes and the table of every route:
mod router;
pub use self::router::*;

// The tests of this file are in files of their own, one for each module, each module with the name it had, so that the path of every test is unchanged (card OA3-06).
// They are declared here, below all the code that is not a test: `docs_gen` reads a file only up to its first `#[cfg(test)]` module.
#[cfg(test)]
mod voice_security_tests;

#[cfg(test)]
mod generation_speed_tests;

#[cfg(test)]
mod pipeline_error_tests;

#[cfg(test)]
mod turn_deadline_tests;

#[cfg(test)]
mod request_deadline_tests;

#[cfg(test)]
mod route_scope_tests;

#[cfg(test)]
mod temp_file_tests;

#[cfg(test)]
mod confirm_answer_tests;

#[cfg(test)]
mod steer_cue_tests;

#[cfg(test)]
mod wake_check_tests;

#[cfg(test)]
mod streamed_turn_tests;

#[cfg(test)]
mod code_and_text_only_tests;
