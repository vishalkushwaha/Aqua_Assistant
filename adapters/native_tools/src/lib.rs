//! Native tools for Aqua's tool host. One cargo feature per domain; each domain is a
//! [`aqua_tool_host::ToolProvider`] that the host registers and guards.

pub mod clipboard;
pub mod contain;
#[cfg(windows)]
pub mod contain_low;
pub mod sandbox;
pub mod secrets;

#[cfg(any(feature = "k8s", feature = "iac", feature = "cloud", feature = "mon"))]
pub mod cli;

#[cfg(feature = "check")]
pub mod check;
#[cfg(feature = "code")]
pub mod code;
#[cfg(feature = "code")]
pub mod lsp;
#[cfg(feature = "db")]
pub mod db;
#[cfg(feature = "doc")]
pub mod doc;
#[cfg(feature = "docker")]
pub mod docker;
#[cfg(feature = "fs")]
pub mod fs;
#[cfg(feature = "forge")]
pub mod forge;
#[cfg(feature = "git")]
pub mod git;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "cloud")]
pub mod cloud;
#[cfg(feature = "iac")]
pub mod iac;
#[cfg(feature = "k8s")]
pub mod k8s;
#[cfg(feature = "mon")]
pub mod mon;
#[cfg(feature = "ssh")]
pub mod ssh;
#[cfg(feature = "sys")]
pub mod sys;
#[cfg(feature = "term")]
pub mod term;
