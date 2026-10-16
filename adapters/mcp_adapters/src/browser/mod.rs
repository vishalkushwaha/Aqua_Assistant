//! Aqua's browser layer: a policy-guarded front for the managed browser (Playwright MCP) and
//! the extension browser (Browser MCP). See `documentation/BROWSER.md`.

pub mod audit;
pub mod backend;
pub mod confirm;
pub mod extract;
pub mod managed;
pub mod observation;
pub mod policy;
pub mod provider;

pub use backend::{BackendKind, BrowserBackend, BrowserReadiness, BrowserStatus};
pub use confirm::{asking_for_run, bind_abandon, is_abandoned, AbandonBinding, AskingForRun, is_pre_approved, is_unattended, pre_approved, PreApprovedGuard, unattended, ConfirmKind, ConfirmRequest, ConfirmWhy, ConfirmationBroker, Decision, Ended, QuestionNote, Resolved, set_question_observer, UnattendedGuard};
pub use managed::{browser_installed, managed_browser_installed, playwright_mcp_cli, BrowserDirs};
pub use observation::{BrowserErrorCode, BrowserObservation};
pub use provider::BrowserProvider;
pub use policy::{requested_channel, url_targets_local_machine, BrowserChannel, BrowserPolicyConfig, JavascriptPolicy, ProfileMode, Resolver, SiteMode, SystemResolver, UrlDecision};
