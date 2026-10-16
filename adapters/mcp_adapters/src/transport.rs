//! What a server of the person's is spoken to through (card OA11-03, task EX-11).
//!
//! A server the person adds is a program Aqua starts and speaks to on its standard input and output (`StdioMcpClient`), or, when the person turned remote servers on and wrote an
//! `https` address, a server on another machine that is spoken to over HTTP (`core/src/mcp_remote.rs`, which holds the HTTP engine of the tool host and so every rule of the
//! network). Everything else about a server is the same whichever it is: the trust (the entry as written), the lock on its tools, the one door every call goes through, the questions,
//! the labels on its answers. That is why [`crate::user_servers::McpProvider`] holds a [`McpTransport`] and not a client.
//!
//! The trait is the part of the client that the provider uses, and nothing more: a transport is not given more of Aqua than that.

use crate::stdio_mcp::{McpServerStatus, McpTool, StdioMcpClient};
use anyhow::Result;
use serde_json::Value;

/// A way of speaking to one MCP server.
pub trait McpTransport: Send {
    /// Starts the server (or opens the session with it) and reads its tools. A server that is started already is not started again.
    fn start(&mut self) -> Result<McpServerStatus>;
    fn is_running(&self) -> bool;
    fn is_initialized(&self) -> bool;
    /// The tools as listed by the last start or listing.
    fn cached_tools(&self) -> &[McpTool];
    /// Lists the tools again.
    fn list_tools(&mut self) -> Result<Vec<McpTool>>;
    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value>;
    /// Ends the server (the process, the session). A later `start` begins again.
    fn shutdown(&mut self) -> Result<()>;
    /// Did the server say (`notifications/tools/list_changed`) that its tools changed since this was last asked? Asking clears it.
    fn take_tools_changed(&mut self) -> bool;
    /// Reads what the server has said while nobody was asking it anything (a change of its tools, a log line): nothing waits for it. A server that ended meanwhile is found out here.
    fn drain_notifications(&mut self);
    /// Records the tools of the last listing as the ones the person accepted (the persistent record, `tool_lock`).
    fn approve_tools(&mut self) -> bool;
    /// Takes back the record that the first look at a server made of a server nobody had accepted.
    fn forget_recorded_tools(&mut self);
    /// From now on only these tools may be called, by the server's own names.
    fn restrict_calls_to(&mut self, names: Vec<String>);
}

impl McpTransport for StdioMcpClient {
    fn start(&mut self) -> Result<McpServerStatus> {
        StdioMcpClient::start(self)
    }

    fn is_running(&self) -> bool {
        StdioMcpClient::is_running(self)
    }

    fn is_initialized(&self) -> bool {
        StdioMcpClient::is_initialized(self)
    }

    fn cached_tools(&self) -> &[McpTool] {
        StdioMcpClient::cached_tools(self)
    }

    fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        StdioMcpClient::list_tools(self)
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        StdioMcpClient::call_tool(self, name, arguments)
    }

    fn shutdown(&mut self) -> Result<()> {
        StdioMcpClient::shutdown(self)
    }

    fn take_tools_changed(&mut self) -> bool {
        StdioMcpClient::take_tools_changed(self)
    }

    fn drain_notifications(&mut self) {
        StdioMcpClient::drain_notifications(self)
    }

    fn approve_tools(&mut self) -> bool {
        StdioMcpClient::approve_tools(self)
    }

    fn forget_recorded_tools(&mut self) {
        StdioMcpClient::forget_recorded_tools(self)
    }

    fn restrict_calls_to(&mut self, names: Vec<String>) {
        StdioMcpClient::restrict_calls_to(self, names)
    }
}
