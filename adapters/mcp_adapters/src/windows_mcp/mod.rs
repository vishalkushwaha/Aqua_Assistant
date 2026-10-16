//! Windows-MCP client — desktop UI automation via `uvx windows-mcp serve`.
//!
//! Upstream: <https://github.com/CursorTouch/Windows-MCP>
//! No git clone required — uses the PyPI package.

pub mod critical;
pub mod guard;
pub mod provider;
pub mod session;
pub mod stop_key;
pub mod tiers;

use crate::stdio_mcp::{StdioMcpClient, StdioMcpConfig};
use guard::WINDOWS_UI_TOOLS;
use std::path::PathBuf;

pub const DEFAULT_WINDOWS_MCP_COMMAND: &str = "uvx";
/// Pinned release. Bump only after re-checking its tool list against [`WINDOWS_UI_TOOLS`].
pub const WINDOWS_MCP_VERSION: &str = "0.8.5";

/// Launch arguments for the pinned server, exposing only the desktop UI tools. The server flag
/// is a first layer; [`guard::WindowsGuard`] enforces the same list on every call.
pub fn pinned_windows_mcp_args() -> Vec<String> {
    vec![
        "--offline".into(),
        "--from".into(),
        format!("windows-mcp=={WINDOWS_MCP_VERSION}"),
        "windows-mcp".into(),
        "serve".into(),
        "--tools".into(),
        WINDOWS_UI_TOOLS.join(","),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_launch_is_pinned_ui_only_and_scrubbed() {
        let cfg = default_windows_config();
        assert!(cfg.direct_spawn && cfg.env_clear);
        let joined = cfg.args.join(" ");
        assert!(joined.contains("windows-mcp==0.8.5"));
        assert!(joined.contains("--offline"));
        assert!(joined.contains("--tools App,Snapshot,Screenshot"));
        for blocked in guard::WINDOWS_BLOCKED_TOOLS {
            assert!(!joined.contains(blocked), "{blocked} must not be in the launch flags");
        }
        assert!(cfg.env.iter().any(|(k, v)| k == "ANONYMIZED_TELEMETRY" && v == "false"));
    }
}

pub type WindowsMcpConfig = StdioMcpConfig;
pub type WindowsMcpClient = StdioMcpClient;

/// Prefer a concrete `uvx` binary so aqua-core works even when `~/.local/bin` is not on PATH.
fn resolve_uvx_command() -> String {
    if let Ok(explicit) = std::env::var("AQUA_WINDOWS_MCP_COMMAND") {
        return explicit;
    }
    resolve_uvx_command_pinned()
}

fn resolve_uvx_command_pinned() -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        candidates.push(PathBuf::from(&userprofile).join(".local").join("bin").join("uvx.exe"));
        candidates.push(PathBuf::from(&userprofile).join(".local").join("bin").join("uvx"));
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(&home).join(".local").join("bin").join("uvx"));
        candidates.push(PathBuf::from(&home).join(".local").join("bin").join("uvx.exe"));
    }
    if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(&local_app).join("uv").join("uvx.exe"));
    }

    for path in candidates {
        if path.is_file() {
            return path.to_string_lossy().into_owned();
        }
    }

    DEFAULT_WINDOWS_MCP_COMMAND.to_string()
}

/// True when a developer asked to override the pinned launch (`AQUA_DEV_UNSAFE=1`).
fn dev_unsafe() -> bool {
    std::env::var("AQUA_DEV_UNSAFE").is_ok_and(|v| v == "1")
}

pub fn default_windows_config() -> StdioMcpConfig {
    let overridden = std::env::var("AQUA_WINDOWS_MCP_ARGS").is_ok() || std::env::var("AQUA_WINDOWS_MCP_COMMAND").is_ok();
    let (command, args) = if overridden && dev_unsafe() {
        tracing::warn!("AQUA_DEV_UNSAFE=1: launching Windows-MCP with an unpinned developer override");
        let args = match std::env::var("AQUA_WINDOWS_MCP_ARGS") {
            Ok(raw) => raw.split_whitespace().map(|s| s.to_string()).collect(),
            Err(_) => pinned_windows_mcp_args(),
        };
        (resolve_uvx_command(), args)
    } else {
        if overridden {
            tracing::warn!("Ignoring AQUA_WINDOWS_MCP_ARGS/COMMAND: set AQUA_DEV_UNSAFE=1 to override the pinned Windows-MCP launch");
        }
        (resolve_uvx_command_pinned(), pinned_windows_mcp_args())
    };
    StdioMcpConfig {
        label: "windows".into(),
        command,
        args,
        cwd: None,
        timeout: std::time::Duration::from_secs(120),
        ready_hint: "Windows-MCP ready. Desktop UI tools are available to the agent.".into(),
        start_hint: "Not started. Install UV (`scripts/setup_windows_mcp.ps1`) then POST /api/v1/windows-mcp/start.".into(),
        // `uvx.exe` is a real executable, so no shell sits between Aqua and the server.
        direct_spawn: true,
        env_clear: true,
        env_allow: Vec::new(),
        env: vec![
            ("ANONYMIZED_TELEMETRY".into(), "false".into()),
            ("WINDOWS_MCP_TOOLS".into(), WINDOWS_UI_TOOLS.join(",")),
        ],
        must_stay_on_loopback: None,
        // Windows-MCP serves both kinds of protocol and takes the one its first message opens: asked `server/discover` it answers as a modern server and then refuses `initialize`
        // (checked). Aqua has always spoken the handshake to it, so it is not asked; whoever moves it to the stateless protocol takes this off.
        known_legacy: true,
    }
}

impl StdioMcpClient {
    pub fn windows_defaults() -> Self {
        Self::new(default_windows_config())
    }
}

pub fn new_windows_client() -> WindowsMcpClient {
    WindowsMcpClient::windows_defaults()
}
