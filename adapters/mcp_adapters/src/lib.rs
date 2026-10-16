//! Multi-Device & Browser / Windows MCP Adapters Suite

pub mod app_launch;
pub mod browser;
pub mod browser_mcp;
pub mod full_access;
#[cfg(windows)]
pub mod job;
pub mod file_index;
pub mod listening;
pub mod local_files;
pub mod restart_budget;
pub mod schema_inline;
pub mod shell_open;
pub mod stdio_mcp;
pub mod tool_lock;
pub mod transport;
pub mod user_servers;
pub mod windows_mcp;

pub use browser_mcp::{
    aqua_workspace_root, check_browser_mcp_health, default_browser_config, ensure_browser_mcp_ready,
    new_browser_client, observation_indicates_extension_offline, open_url_in_default_browser,
    probe_browser_action, run_browser_mcp_setup_script, BrowserMcpClient, BrowserMcpConfig,
    BrowserMcpFailStage, BrowserMcpHealth, BrowserMcpStatus,
    EnsureBrowserMcpOptions,
    BROWSER_MCP_PACKAGE, DEFAULT_BROWSER_MCP_ARGS, DEFAULT_BROWSER_MCP_COMMAND,
    EXTENSION_SETUP_URL,
};
#[cfg(feature = "unwired")]
pub use browser_mcp::{CommentNotification, SocialWatchdog, SocialWatchdogConfig, UploadTask};

pub use stdio_mcp::{tool_lines, tools_prompt_block, McpServerStatus, McpTool, StdioMcpClient, StdioMcpConfig};
pub use transport::McpTransport;

pub use windows_mcp::provider::WindowsUiProvider;
pub use shell_open::{is_safe_web_url, open_web_url};
pub use windows_mcp::guard::{
    filter_tools as filter_windows_tools, Caller as WindowsCaller, WindowsGuard, WINDOWS_BLOCKED_TOOLS,
    WINDOWS_UI_TOOLS,
};
pub use windows_mcp::{
    default_windows_config, new_windows_client, WindowsMcpClient, WindowsMcpConfig,
    pinned_windows_mcp_args, DEFAULT_WINDOWS_MCP_COMMAND, WINDOWS_MCP_VERSION,
};

