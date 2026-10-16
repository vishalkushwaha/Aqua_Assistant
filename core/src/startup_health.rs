//! Startup health checks + auto-repair for Aqua Core dependencies.

pub mod early;

use anyhow::{Context, Result};
use serde::Serialize;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use aqua_audio_adapter::{
    deepfilter_cli, whisper_cli, KokoroTtsEngine, NoiseSuppressor, WhisperSttEngine,
};
use aqua_mcp_adapters::browser::{BackendKind, BrowserBackend};
use aqua_mcp_adapters::WindowsMcpClient;
use crate::modules::llm_router::LlamaRouterEngine;
use crate::runtime::ModelsInventory;

#[derive(Debug, Clone, Serialize)]
pub struct ComponentHealth {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub auto_fixed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartupHealthReport {
    pub ready: bool,
    pub checked_at_unix_ms: u128,
    pub duration_ms: u64,
    pub components: Vec<ComponentHealth>,
    pub blockers: Vec<String>,
    /// When the engine checks in this report were last repeated (the rest is as it was at startup).
    #[serde(default)]
    pub rechecked_at_unix_ms: u128,
}

/// The components whose failure means Aqua cannot do its main job.
const REQUIRED: [&str; 5] = ["whisper_cli", "llama_engine", "kokoro_engine", "kokoro_smoke", "models_inventory"];

fn blockers_of(components: &[ComponentHealth]) -> Vec<String> {
    components.iter().filter(|c| !c.ok && REQUIRED.contains(&c.name.as_str())).map(|c| format!("{}: {}", c.name, c.detail)).collect()
}

impl StartupHealthReport {
    /// The startup report with the engine checks repeated now. A model deleted, or downloaded, after startup changes
    /// the answer; the slow checks (the speech smoke test, the browser, the daemon) keep their startup result.
    pub fn rechecked(mut self, whisper: bool, llm: bool, tts: bool) -> Self {
        for c in &mut self.components {
            let live = match c.name.as_str() {
                "whisper_cli" => whisper,
                "llama_engine" => llm,
                "kokoro_engine" => tts,
                _ => continue,
            };
            if c.ok != live {
                c.ok = live;
                c.detail = if live { "ready (rechecked)".into() } else { "no longer available (rechecked)".into() };
            }
        }
        self.blockers = blockers_of(&self.components);
        self.ready = whisper && llm && tts && self.blockers.is_empty();
        self.rechecked_at_unix_ms = now_unix_ms();
        self
    }
}

pub struct ManagedServices {
    pub kokoro_daemon: Mutex<Option<Child>>,
}

impl Default for ManagedServices {
    fn default() -> Self {
        Self {
            kokoro_daemon: Mutex::new(None),
        }
    }
}

impl Drop for ManagedServices {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.kokoro_daemon.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---- the order of a start (card R3-07; card OA3-01, AQ-04) ----------------------------------------------------------------------------------------------------------------

/// The steps of the start of the core that have to come in a fixed order, in that order (`main` takes them one after the other). The port is looked at and then taken first, so that
/// a second Aqua ends at once and a client that asks during the slow work is answered "starting" (card R3-07); the record of runs and the database are opened before an engine is
/// made; the model is loaded and warmed (`Warmup`) while the speech engine and the checks that follow start; the port is handed to the real gateway after the checks, and serving is
/// last. A step that is skipped (`PortTaken` when bind-first is off, `HandOver` when the gateway binds the port itself) leaves the others where they are.
///
/// Before card OA3-01 (AQ-04) this order was kept true by a test that searched the text of `main.rs` for the line of each step and compared where they stood. `main` now says which step
/// it has reached ([`StartupTrace::note`]), and a step that comes out of order, or twice, is refused by the trace; this list is the one both read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StartupStep {
    /// Another Aqua on the port? (`runtime::port_in_use`)
    PortChecked,
    /// The port is taken and answers "starting" (`early::begin`), when bind-first is on.
    PortTaken,
    /// The record of agent runs is opened.
    AgentRuns,
    /// The database is opened.
    Database,
    /// The noise suppressor is made.
    NoiseSuppressor,
    /// The speech-to-text engine is made.
    Whisper,
    /// The language model engine is made.
    Llama,
    /// The model is loaded and its prompts are read (`start_warmup`), in the background.
    Warmup,
    /// The text-to-speech engine is made.
    Kokoro,
    /// The checks of the dependencies are run.
    Health,
    /// The port is handed to the real gateway, when bind-first took it.
    HandOver,
    /// The gateway serves.
    Serving,
}

impl StartupStep {
    /// Every step, in the order of the start.
    pub const ALL: [StartupStep; 12] = [
        StartupStep::PortChecked,
        StartupStep::PortTaken,
        StartupStep::AgentRuns,
        StartupStep::Database,
        StartupStep::NoiseSuppressor,
        StartupStep::Whisper,
        StartupStep::Llama,
        StartupStep::Warmup,
        StartupStep::Kokoro,
        StartupStep::Health,
        StartupStep::HandOver,
        StartupStep::Serving,
    ];

    /// Does a start without bind-first, or one whose port could not be handed over, leave this step out?
    pub const fn is_optional(self) -> bool {
        matches!(self, StartupStep::PortTaken | StartupStep::HandOver)
    }
}

/// The steps of the start in the order `main` takes them (see [`StartupStep`]).
pub fn ordered_startup_steps() -> &'static [StartupStep] {
    &StartupStep::ALL
}

/// What was wrong with a step that was reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupMistake {
    /// The step was reported already.
    Twice(StartupStep),
    /// The step comes before one that was reported already (`after` is the last).
    TooLate { step: StartupStep, after: StartupStep },
    /// A step that a start does not leave out comes before this one in the order and was never reported.
    Skipped { step: StartupStep, missing: StartupStep },
}

impl std::fmt::Display for StartupMistake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartupMistake::Twice(step) => write!(f, "the start reached {step:?} twice"),
            StartupMistake::TooLate { step, after } => write!(f, "the start reached {step:?} after {after:?}, which comes later in the order of the start"),
            StartupMistake::Skipped { step, missing } => write!(f, "the start reached {step:?} without having reached {missing:?}, which comes before it in the order of the start"),
        }
    }
}

/// What `main` has done of its start so far. It reports each step as it comes to it ([`note`](Self::note)); a step that comes before one already reported, or that was reported
/// already, is refused ([`reached`](Self::reached)).
#[derive(Debug, Default)]
pub struct StartupTrace {
    reached: Vec<StartupStep>,
}

impl StartupTrace {
    pub fn new() -> Self {
        Self::default()
    }

    /// The start has come to `step`. `Err` when it was reported already, when it comes before a step that was, or when a step that no start leaves out and that comes before it was
    /// never reported; the step is then not recorded.
    pub fn reached(&mut self, step: StartupStep) -> Result<(), StartupMistake> {
        if self.reached.contains(&step) {
            return Err(StartupMistake::Twice(step));
        }
        if let Some(&after) = self.reached.last() {
            if step < after {
                return Err(StartupMistake::TooLate { step, after });
            }
        }
        if let Some(missing) = StartupStep::ALL.into_iter().find(|s| *s < step && !s.is_optional() && !self.reached.contains(s)) {
            return Err(StartupMistake::Skipped { step, missing });
        }
        self.reached.push(step);
        Ok(())
    }

    /// [`reached`](Self::reached) for `main`: a step out of its place is a mistake of whoever changed the start, not a reason to stop Aqua, so it is written to the log as an error
    /// and, in a build with assertions (a development build), it stops the start so that it is seen at once.
    pub fn note(&mut self, step: StartupStep) {
        if let Err(e) = self.reached(step) {
            warn!("start-up order: {e}");
            debug_assert!(false, "start-up order: {e}");
        }
    }

    /// The steps reported so far, in the order they were reported.
    pub fn steps(&self) -> &[StartupStep] {
        &self.reached
    }
}

/// Wait until the speech engine's daemon answers, but not longer than `max`. (It is ready about 8 seconds after a start.)
pub fn wait_for_kokoro_daemon(max: Duration) -> bool {
    let deadline = Instant::now() + max;
    while Instant::now() < deadline {
        if kokoro_daemon_healthy() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    kokoro_daemon_healthy()
}

fn kokoro_daemon_healthy() -> bool {
    TcpStream::connect_timeout(
        &"127.0.0.1:8766".parse().expect("static addr"),
        Duration::from_millis(250),
    )
    .ok()
    .and_then(|mut stream| {
        use std::io::{Read, Write};
        let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
        let _ = stream.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1:8766\r\nConnection: close\r\n\r\n");
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf);
        // The status line, not any "200" anywhere in the reply (a port number, a byte count...).
        Some(text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200"))
    })
    .unwrap_or(false)
}

fn resolve_kokoro_daemon_python(vendor: &Path) -> Option<PathBuf> {
    let candidates = [
        vendor.join(".venv/Scripts/python.exe"),
        vendor.join(".venv/bin/python"),
        // An installed copy carries its own Python (scripts/package_kokoro_portable.ps1).
        vendor.join("python/python.exe"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// The folder holding `kokoro_daemon.py`: the development checkout when it has a working environment, else the installed `engines\kokoro`.
fn resolve_kokoro_vendor(models_dir: &Path) -> PathBuf {
    let checkout = models_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("vendor/kokoro-tts"))
        .unwrap_or_else(|| PathBuf::from("vendor/kokoro-tts"));
    if resolve_kokoro_daemon_python(&checkout).is_some() {
        return checkout;
    }
    aqua_audio_adapter::engines::installed_engine_dir("kokoro")
        .filter(|d| resolve_kokoro_daemon_python(d).is_some())
        .unwrap_or(checkout)
}

fn start_kokoro_daemon(managed: &ManagedServices, models_dir: &Path) -> ComponentHealth {
    if kokoro_daemon_healthy() {
        return ComponentHealth {
            name: "kokoro_daemon".into(),
            ok: true,
            detail: "Already listening on 127.0.0.1:8766".into(),
            auto_fixed: false,
        };
    }

    let vendor = resolve_kokoro_vendor(models_dir);
    let script = vendor.join("kokoro_daemon.py");
    let python = match resolve_kokoro_daemon_python(&vendor) {
        Some(p) => p,
        None => {
            return ComponentHealth {
                name: "kokoro_daemon".into(),
                ok: false,
                detail: format!(
                    "Kokoro daemon not running and venv python missing under {}",
                    vendor.display()
                ),
                auto_fixed: false,
            };
        }
    };
    if !script.is_file() {
        return ComponentHealth {
            name: "kokoro_daemon".into(),
            ok: false,
            detail: format!("Missing {}", script.display()),
            auto_fixed: false,
        };
    }

    info!("Auto-starting Kokoro TTS daemon via {}", python.display());
    let mut daemon = Command::new(&python);
    daemon.arg(&script).current_dir(&vendor);
    // The daemon looks for the model beside the checkout; an installed copy keeps it in the data folder.
    if std::env::var_os("AQUA_KOKORO_MODELS").is_none() {
        daemon.env("AQUA_KOKORO_MODELS", models_dir.join("kokoro"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        daemon.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashing up for an installed app
    }
    match daemon
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            {
                let mut guard = managed.kokoro_daemon.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                *guard = Some(child);
            }
            let deadline = Instant::now() + Duration::from_secs(45);
            while Instant::now() < deadline {
                if kokoro_daemon_healthy() {
                    return ComponentHealth {
                        name: "kokoro_daemon".into(),
                        ok: true,
                        detail: "Started and healthy on 127.0.0.1:8766".into(),
                        auto_fixed: true,
                    };
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            ComponentHealth {
                name: "kokoro_daemon".into(),
                ok: false,
                detail: "Spawned kokoro_daemon.py but /health did not become ready within 45s".into(),
                auto_fixed: false,
            }
        }
        Err(e) => ComponentHealth {
            name: "kokoro_daemon".into(),
            ok: false,
            detail: format!("Failed to spawn kokoro daemon: {e}"),
            auto_fixed: false,
        },
    }
}

fn check_cli(name: &str, present: bool, path_hint: String) -> ComponentHealth {
    ComponentHealth {
        name: name.into(),
        ok: present,
        detail: if present {
            format!("Found: {path_hint}")
        } else {
            format!("Missing: {path_hint}")
        },
        auto_fixed: false,
    }
}

/// Run full startup verification. Auto-starts Kokoro daemon and MCP servers when safe.
pub fn run_startup_health(
    models: &ModelsInventory,
    models_dir: &Path,
    noise: &NoiseSuppressor,
    whisper: &WhisperSttEngine,
    llm: &LlamaRouterEngine,
    tts: &KokoroTtsEngine,
    browser: &BrowserBackend,
    windows_mcp: &mut WindowsMcpClient,
    managed: &ManagedServices,
    start_mcp: bool,
) -> StartupHealthReport {
    let started = Instant::now();
    let mut components = Vec::new();
    let mut blockers = Vec::new();

    components.push(ComponentHealth {
        name: "models_inventory".into(),
        ok: models.whisper_ggml && models.llm_gguf && models.kokoro_onnx,
        detail: format!(
            "whisper={} deepfilter={} llm={} kokoro={}",
            models.whisper_ggml, models.deepfilternet_onnx, models.llm_gguf, models.kokoro_onnx
        ),
        auto_fixed: false,
    });

    let whisper_cli = whisper_cli::resolve_cli_binary();
    components.push(check_cli(
        "whisper_cli",
        whisper_cli.is_some() && whisper.is_engine_ready(),
        whisper_cli
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "whisper-cli".into()),
    ));

    let df_cli = deepfilter_cli::resolve_cli_binary();
    components.push(check_cli(
        "deepfilter_cli",
        df_cli.is_some() && noise.is_engine_ready(),
        df_cli
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "deep-filter".into()),
    ));

    components.push(ComponentHealth {
        name: "llama_engine".into(),
        ok: llm.is_engine_ready(),
        detail: if llm.is_engine_ready() {
            "GGUF + llama-cli ready".into()
        } else {
            "LLM engine not ready".into()
        },
        auto_fixed: false,
    });

    components.push(ComponentHealth {
        name: "kokoro_engine".into(),
        ok: tts.is_engine_ready(),
        detail: if tts.is_engine_ready() {
            "ONNX + voices + CLI ready".into()
        } else {
            "Kokoro TTS engine not ready".into()
        },
        auto_fixed: false,
    });

    let daemon = start_kokoro_daemon(managed, models_dir);
    if !daemon.ok {
        warn!("Kokoro daemon unhealthy: {}", daemon.detail);
    }
    components.push(daemon);

    // Optional live TTS smoke (daemon or cold CLI). Non-blocking for readiness if CLI works.
    if tts.is_engine_ready() {
        match tts.synthesize_wav("Ready.", "af_sarah", "en-us") {
            Ok(wav) if wav.len() > 44 => components.push(ComponentHealth {
                name: "kokoro_smoke".into(),
                ok: true,
                detail: format!("Synthesized {} WAV bytes", wav.len()),
                auto_fixed: false,
            }),
            Ok(_) => {
                components.push(ComponentHealth {
                    name: "kokoro_smoke".into(),
                    ok: false,
                    detail: "TTS returned empty WAV".into(),
                    auto_fixed: false,
                });
                blockers.push("kokoro_smoke failed".into());
            }
            Err(e) => {
                components.push(ComponentHealth {
                    name: "kokoro_smoke".into(),
                    ok: false,
                    detail: format!("TTS smoke failed: {e}"),
                    auto_fixed: false,
                });
                blockers.push("kokoro_smoke failed".into());
            }
        }
    }

    if start_mcp {
        match windows_mcp.start() {
            Ok(status) => components.push(ComponentHealth {
                name: "windows_mcp".into(),
                ok: status.initialized,
                detail: format!(
                    "{} tools (server {:?})",
                    status.tool_count, status.server_name
                ),
                auto_fixed: true,
            }),
            Err(e) => components.push(ComponentHealth {
                name: "windows_mcp".into(),
                ok: false,
                detail: format!("Could not start Windows-MCP (non-blocking): {e}"),
                auto_fixed: false,
            }),
        }

    }

    // The managed browser starts lazily on the first browser task (it costs ~300 MB while open);
    // here we only verify it is installed and clear browsers orphaned by a previous crash.
    match browser.kind() {
        BackendKind::Managed => {
            browser.sweep_stale_browsers();
            let installed = browser.managed_installed();
            let policy = browser.policy();
            components.push(ComponentHealth {
                name: "managed_browser".into(),
                ok: installed,
                detail: if installed {
                    format!(
                        "Playwright MCP installed; {} opens on first use ({:?} profile, site mode {:?})",
                        policy.browser.as_arg(),
                        policy.profile,
                        policy.mode
                    )
                } else {
                    "Not installed (non-blocking) — run scripts/setup-managed-browser.ps1".into()
                },
                auto_fixed: false,
            });
        }
        BackendKind::Extension if start_mcp => {
            let client = browser.extension_client();
            let mut c = client.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match c.start() {
                Ok(status) => components.push(ComponentHealth {
                    name: "browser_mcp".into(),
                    ok: status.initialized,
                    detail: format!(
                        "{} tools — connect Chrome extension before browser automation",
                        status.tool_count
                    ),
                    auto_fixed: true,
                }),
                Err(e) => components.push(ComponentHealth {
                    name: "browser_mcp".into(),
                    ok: false,
                    detail: format!("Could not start Browser MCP (non-blocking): {e}"),
                    auto_fixed: false,
                }),
            }
        }
        BackendKind::Extension => {}
    }

    blockers.extend(blockers_of(&components));

    // DeepFilter is optional for voice path (pipeline falls back to raw WAV).
    // A required component that failed (the speech smoke test, the models inventory) is a blocker even when each
    // engine reports itself ready: ready means no blockers.
    let ready = whisper.is_engine_ready() && llm.is_engine_ready() && tts.is_engine_ready() && blockers.is_empty();

    let report = StartupHealthReport {
        ready,
        checked_at_unix_ms: now_unix_ms(),
        duration_ms: started.elapsed().as_millis() as u64,
        components,
        blockers,
        rechecked_at_unix_ms: now_unix_ms(),
    };

    if ready {
        info!(
            "Startup health READY in {}ms ({} components)",
            report.duration_ms,
            report.components.len()
        );
    } else {
        warn!(
            "Startup health NOT READY in {}ms — blockers: {:?}",
            report.duration_ms, report.blockers
        );
    }

    report
}

pub fn ensure_workspace_dirs(models_dir: &Path) -> Result<()> {
    let scratch = models_dir
        .parent()
        .unwrap_or(models_dir)
        .join("generated_images");
    std::fs::create_dir_all(models_dir).context("create models dir")?;
    let _ = std::fs::create_dir_all(scratch);
    // Laya gatekeeper home (LAYA_HOME) — models + learning stay under Aqua.
    let laya = models_dir.join("laya");
    let _ = std::fs::create_dir_all(laya.join("models"));
    let _ = std::fs::create_dir_all(laya.join("learning"));
    Ok(())
}

#[cfg(test)]
mod recheck_tests {
    use super::*;

    fn report(whisper_ok: bool) -> StartupHealthReport {
        let c = |name: &str, ok: bool| ComponentHealth { name: name.into(), ok, detail: "at start".into(), auto_fixed: false };
        let components = vec![c("whisper_cli", whisper_ok), c("llama_engine", true), c("kokoro_engine", true), c("kokoro_smoke", true), c("deepfilter_cli", false)];
        let blockers = blockers_of(&components);
        StartupHealthReport { ready: blockers.is_empty(), checked_at_unix_ms: 1, duration_ms: 1, components, blockers, rechecked_at_unix_ms: 1 }
    }

    #[test]
    fn a_ready_report_stops_saying_ready_when_an_engine_goes_away() {
        let r = report(true);
        assert!(r.ready && r.blockers.is_empty());
        let later = r.rechecked(false, true, true);
        assert!(!later.ready);
        assert!(later.blockers.iter().any(|b| b.starts_with("whisper_cli")), "{:?}", later.blockers);
    }

    #[test]
    fn a_model_that_arrives_after_start_clears_the_blocker_and_optional_parts_never_block() {
        let r = report(false);
        assert!(!r.ready && r.blockers.len() == 1, "the broken deepfilter is optional: {:?}", r.blockers);
        let later = r.rechecked(true, true, true);
        assert!(later.ready && later.blockers.is_empty());
        assert!(later.rechecked_at_unix_ms > 1);
    }
}
