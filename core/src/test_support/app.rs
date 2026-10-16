//! `app_state`: the gateway's state (`AppState`) for a test that asks a route of the real route table, with no model, no speech engine and no process (card OA3-01, AQ-04).
//!
//! Several tests used to check a handler by searching the server's source for the call they expected ("this handler asks for the UI token", "this route is wired to that
//! handler"). A route is asked now: `api_routes()` is the table `run_api_gateway_on` serves, and a test puts the same gate over it (`require_local_auth`) and gives it this state.
//!
//! Every engine is made over an empty folder of the world, so each says it is not ready and none starts anything (their constructors only look at what is there); the language
//! model, the speech engines and the browser are never started. The tool host is the one the test gives (its providers, its hooks, its ledger): that is what a route of the tools
//! reaches. The credential store is in memory.

use crate::api_gateway::server::AppState;
use crate::laya_validator::LayaGate;
use crate::modules::llm_router::LlamaRouterEngine;
use crate::startup_health::ManagedServices;
use crate::storage::DatabaseManager;
use crate::test_support::TempWorld;
use aqua_audio_adapter::{KokoroTtsEngine, NoiseSuppressor, VoiceBiometricsEngine, WhisperSttEngine};
use aqua_mcp_adapters::browser::BrowserBackend;
use aqua_mcp_adapters::{new_browser_client, new_windows_client};
use aqua_native_tools::secrets::MemorySecrets;
use aqua_tool_host::ToolHost;
use std::sync::{Arc, Mutex};

/// The state of a gateway over the folders of `world`, whose tool host is `tools`.
pub fn app_state(world: &TempWorld, tools: Arc<ToolHost>) -> AppState {
    let models = world.root.join("models");
    AppState {
        models_dir: models.clone(),
        db: Arc::new(DatabaseManager::with_secrets(world.root.join("db"), Arc::new(MemorySecrets::new())).expect("the test database opens")),
        noise: Arc::new(Mutex::new(NoiseSuppressor::new(models.join("deepfilternet")).expect("an empty folder is not an error"))),
        whisper: Arc::new(WhisperSttEngine::new(models.join("whisper")).expect("an empty folder is not an error")),
        llm: Arc::new(LlamaRouterEngine::new(models.join("llm")).expect("an empty folder is not an error")),
        tts: Arc::new(KokoroTtsEngine::new(models.join("kokoro")).expect("an empty folder is not an error")),
        biometrics: Arc::new(Mutex::new(VoiceBiometricsEngine::new(models.join("biometrics")).expect("an empty folder is not an error"))),
        browser: Arc::new(BrowserBackend::new(&world.root.join("browser"), new_browser_client())),
        windows_mcp: Arc::new(Mutex::new(new_windows_client())),
        tools,
        laya: Arc::new(LayaGate::disabled_heuristic(world.root.join("laya"))),
        active_session_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        managed_services: Arc::new(ManagedServices::default()),
        startup_report: Arc::new(Mutex::new(None)),
    }
}
