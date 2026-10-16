//! The shared state of the gateway (card OA3-06, moved out of `server.rs`): `AppState`, what every handler is given: the engines, the database, the browser layer, the tool host and the counter of the voice session.

use super::*;

#[derive(Clone)]
pub struct AppState {
    pub models_dir: PathBuf,
    pub db: Arc<DatabaseManager>,
    pub noise: Arc<Mutex<NoiseSuppressor>>,
    pub whisper: Arc<WhisperSttEngine>,
    pub llm: Arc<LlamaRouterEngine>,
    pub tts: Arc<KokoroTtsEngine>,
    pub biometrics: Arc<Mutex<VoiceBiometricsEngine>>,
    /// Policy-guarded browser layer (managed Playwright browser or the Browser MCP extension).
    pub browser: Arc<BrowserBackend>,
    pub windows_mcp: Arc<Mutex<WindowsMcpClient>>,
    /// The guarded tool host: every tool call from the API goes through it.
    pub tools: Arc<aqua_tool_host::ToolHost>,
    pub laya: Arc<LayaGate>,
    pub active_session_id: Arc<std::sync::atomic::AtomicU64>,
    pub managed_services: Arc<ManagedServices>,
    pub startup_report: Arc<Mutex<Option<StartupHealthReport>>>,
}
