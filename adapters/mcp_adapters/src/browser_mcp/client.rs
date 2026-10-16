//! Browser MCP — thin wrapper around [`crate::stdio_mcp::StdioMcpClient`].
//!
//! Upstream: <https://github.com/BrowserMCP/mcp>

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::health::aqua_workspace_root;
use crate::stdio_mcp::{McpServerStatus, McpTool, StdioMcpClient, StdioMcpConfig};

pub const DEFAULT_BROWSER_MCP_COMMAND: &str = "npx";
/// Pinned: `npx -y` runs this package with the person's browser and a user-level token in reach, so a new release must
/// be chosen on purpose (update this version after reading what changed), never picked up silently by `@latest`.
pub const DEFAULT_BROWSER_MCP_ARGS: &[&str] = &["-y", "@browsermcp/mcp@0.1.3"];

/// The port the package opens its WebSocket on (`defaultWsPort` in its source). It opens it on every interface and checks nobody (see `listening`), so Aqua refuses
/// to keep it running unless the person accepts that.
pub const BROWSER_MCP_PORT: u16 = 9009;

/// The file, under the workspace root, that makes the server listen on 127.0.0.1 only (see `tools/browser-mcp/loopback_only.cjs`): the package opens its WebSocket without an
/// address, so on every interface. Aqua starts the server with `NODE_OPTIONS=--require=<this file>`; without the file the server starts as it always did and the check in
/// `listening` refuses it when it listens on the network.
pub const LOOPBACK_SHIM: &str = "tools/browser-mcp/loopback_only.cjs";

/// `NODE_OPTIONS` for the server: what the environment already has, then `--require` of the shim. The path has forward slashes (Node reads a backslash in this variable as an
/// escape) and is quoted when it holds a space. Pure.
pub fn node_options_with_shim(existing: Option<&str>, shim: &Path) -> String {
    let path = shim.to_string_lossy().replace('\\', "/");
    let require = if path.contains(' ') { format!("--require=\"{path}\"") } else { format!("--require={path}") };
    match existing.map(str::trim).filter(|e| !e.is_empty()) {
        Some(existing) => format!("{existing} {require}"),
        None => require,
    }
}

/// The id of the one browser extension that may open the server's WebSocket (audit item SE-05), from the person's settings: 32 letters a-p as `chrome://extensions` shows it. When it
/// is set the shim lets in only `Origin: chrome-extension://<this id>`; without it any extension is let in and a web page never is.
pub const EXTENSION_ID_ENV: &str = "AQUA_BROWSER_MCP_EXTENSION_ID";

/// With this set to `1` the shim refuses a second connection while one is open, instead of letting the newest take the first one's place. Off unless the person sets it: the
/// extension may open a new connection for another tab before the old one has closed.
pub const ONE_CONNECTION_ENV: &str = "AQUA_BROWSER_MCP_ONE_CONNECTION";

/// The extension id as the shim wants it (trimmed, lower case), or `None` for a value that is not a Chrome extension id: a mistyped id is dropped here, with the shim's own
/// note, rather than refusing the real extension for ever. Pure.
pub fn extension_id_from(raw: Option<&str>) -> Option<String> {
    let id = raw?.trim().to_ascii_lowercase();
    (id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))).then_some(id)
}

/// The variables of the Origin gate: the extension id when it is valid, and the one-connection switch when it is on. Pure.
pub fn origin_gate_env(extension_id: Option<&str>, one_connection: Option<&str>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Some(id) = extension_id_from(extension_id) {
        env.push((EXTENSION_ID_ENV.to_string(), id));
    }
    if one_connection.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes")) {
        env.push((ONE_CONNECTION_ENV.to_string(), "1".to_string()));
    }
    env
}

/// The variables that put the server on the loopback address and keep web pages from opening its WebSocket, when the shim is there.
fn loopback_env() -> Vec<(String, String)> {
    let shim = aqua_workspace_root().join(LOOPBACK_SHIM);
    if !shim.is_file() {
        return Vec::new();
    }
    let mut env = vec![
        ("NODE_OPTIONS".to_string(), node_options_with_shim(std::env::var("NODE_OPTIONS").ok().as_deref(), &shim)),
        ("AQUA_LOOPBACK_ONLY_PORT".to_string(), BROWSER_MCP_PORT.to_string()),
    ];
    env.extend(origin_gate_env(std::env::var(EXTENSION_ID_ENV).ok().as_deref(), std::env::var(ONE_CONNECTION_ENV).ok().as_deref()));
    env
}

pub type BrowserMcpConfig = StdioMcpConfig;
pub type BrowserMcpStatus = McpServerStatus;
pub type BrowserMcpClient = StdioMcpClient;

pub fn default_browser_config() -> StdioMcpConfig {
    let command = std::env::var("AQUA_BROWSER_MCP_COMMAND")
        .unwrap_or_else(|_| DEFAULT_BROWSER_MCP_COMMAND.to_string());
    let args = if let Ok(raw) = std::env::var("AQUA_BROWSER_MCP_ARGS") {
        raw.split_whitespace().map(|s| s.to_string()).collect()
    } else {
        DEFAULT_BROWSER_MCP_ARGS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    };
    StdioMcpConfig {
        label: "browser".into(),
        command,
        args,
        cwd: None,
        timeout: std::time::Duration::from_secs(60),
        ready_hint: "Browser MCP ready. Connect the Chrome extension on a tab.".into(),
        start_hint: "Not started. POST /api/v1/browser-mcp/start (needs Node + extension).".into(),
        direct_spawn: false,
        env_clear: false,
        env_allow: Vec::new(),
        env: loopback_env(),
        must_stay_on_loopback: Some(BROWSER_MCP_PORT),
        // Browser MCP 0.1.3 answers `server/discover` with "Method not found" (checked): it speaks the handshake, which Aqua pins it to.
        known_legacy: true,
    }
}

impl StdioMcpClient {
    pub fn browser_defaults() -> Self {
        Self::new(default_browser_config())
    }
}

/// Back-compat constructor used by aqua-core.
pub fn new_browser_client() -> BrowserMcpClient {
    BrowserMcpClient::browser_defaults()
}

pub fn browser_start(client: &mut BrowserMcpClient) -> Result<BrowserMcpStatus> {
    client.start()
}

pub fn browser_call(client: &mut BrowserMcpClient, tool: &str, args: Value) -> Result<Value> {
    client.call_tool(tool, args)
}

pub fn browser_tools(client: &mut BrowserMcpClient) -> Result<Vec<McpTool>> {
    client.list_tools()
}

#[cfg(test)]
mod loopback_tests {
    use super::*;

    #[test]
    fn the_server_is_started_with_the_shim_required_and_what_node_options_held_stays() {
        let shim = Path::new(r"E:\Personal_AI\Aqua\tools\browser-mcp\loopback_only.cjs");
        assert_eq!(node_options_with_shim(None, shim), "--require=E:/Personal_AI/Aqua/tools/browser-mcp/loopback_only.cjs");
        assert_eq!(node_options_with_shim(Some("  "), shim), "--require=E:/Personal_AI/Aqua/tools/browser-mcp/loopback_only.cjs", "a blank value is none");
        assert_eq!(
            node_options_with_shim(Some("--max-old-space-size=2048"), shim),
            "--max-old-space-size=2048 --require=E:/Personal_AI/Aqua/tools/browser-mcp/loopback_only.cjs"
        );
        let spaced = Path::new(r"C:\Users\a b\Aqua\tools\browser-mcp\loopback_only.cjs");
        assert_eq!(node_options_with_shim(None, spaced), "--require=\"C:/Users/a b/Aqua/tools/browser-mcp/loopback_only.cjs\"", "a space needs the quotes");
    }

    #[test]
    fn the_extension_id_reaches_the_server_only_when_it_is_a_real_extension_id() {
        let id = "abcdefghijklmnopabcdefghijklmnop";
        assert_eq!(extension_id_from(Some(id)).as_deref(), Some(id));
        assert_eq!(extension_id_from(Some("  ABCDEFGHIJKLMNOPabcdefghijklmnop \n")).as_deref(), Some(id), "trimmed and lower case");
        for bad in ["", "   ", "short", "abcdefghijklmnopabcdefghijklmnopq", "abcdefghijklmnopabcdefghijklmnoq", "abcdefghijklmnopabcdefghijklmno1", "chrome-extension://abcdefghijklmnopabcdefghijklmnop"] {
            assert_eq!(extension_id_from(Some(bad)), None, "{bad:?}");
        }
        assert_eq!(extension_id_from(None), None);
        assert_eq!(origin_gate_env(Some(id), None), vec![(EXTENSION_ID_ENV.to_string(), id.to_string())]);
        assert!(origin_gate_env(Some("nonsense"), None).is_empty(), "a mistyped id is not passed on");
        assert!(origin_gate_env(None, None).is_empty(), "nothing is set, so nothing is passed: web pages are kept out anyway");
    }

    #[test]
    fn one_connection_is_passed_on_only_when_the_person_switches_it_on() {
        for on in ["1", "on", "ON", " true ", "yes"] {
            assert_eq!(origin_gate_env(None, Some(on)), vec![(ONE_CONNECTION_ENV.to_string(), "1".to_string())], "{on:?}");
        }
        for off in ["", "0", "off", "no", "2"] {
            assert!(origin_gate_env(None, Some(off)).is_empty(), "{off:?}");
        }
    }

    #[test]
    fn the_shim_the_launch_names_is_in_the_repository() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let shim = root.join(LOOPBACK_SHIM);
        assert!(shim.is_file(), "{} is missing", shim.display());
        let text = std::fs::read_to_string(&shim).unwrap();
        assert!(text.contains("AQUA_LOOPBACK_ONLY_PORT") && text.contains("127.0.0.1") && text.contains("net.Server.prototype.listen"));
        // The Origin gate reads the variables this file passes on.
        assert!(text.contains(EXTENSION_ID_ENV) && text.contains(ONE_CONNECTION_ENV) && text.contains("'upgrade'"));
    }
}
