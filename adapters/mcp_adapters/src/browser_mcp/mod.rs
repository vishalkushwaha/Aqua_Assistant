pub mod client;
pub mod health;
#[cfg(feature = "unwired")]
pub mod social_watchdog;

pub use client::{
    default_browser_config, new_browser_client, BrowserMcpClient, BrowserMcpConfig,
    BrowserMcpStatus, DEFAULT_BROWSER_MCP_ARGS, DEFAULT_BROWSER_MCP_COMMAND,
};
pub use health::{
    check_browser_mcp_health, ensure_browser_mcp_ready, observation_indicates_extension_offline,
    open_url_in_default_browser, probe_browser_action, run_browser_mcp_setup_script,
    aqua_workspace_root, find_repo_root, resolve_workspace_root, BrowserMcpFailStage, BrowserMcpHealth, EnsureBrowserMcpOptions,
    BROWSER_MCP_PACKAGE, EXTENSION_SETUP_URL,
};
#[cfg(feature = "unwired")]
pub use social_watchdog::{
    CommentNotification, SocialWatchdog, SocialWatchdogConfig, UploadTask,
};
