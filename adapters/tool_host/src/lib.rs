//! Aqua's tool host: one guarded path from any caller to any tool.
//!
//! Providers (native Rust tools, or adapters around external MCP servers) describe their tools
//! with a [`ToolManifest`] and implement [`ToolProvider`]. Everything else lives here: argument
//! validation, the permission decision, approvals, the timeout and cancellation, output
//! sanitising and secret redaction, and a hash-chained audit ledger. A provider never talks to
//! the model or the user; the host does, in the same way for every tool.

pub mod audit;
pub mod canon;
pub mod config_text;
pub mod envelope;
pub mod explain;
pub mod fsutil;
pub mod heads;
pub mod hooks;
pub mod host;
pub mod manifest;
pub mod merkle;
pub mod net_addr;
pub mod output;
pub mod policy;
pub mod preapproval;
pub mod progress;
pub mod proof;
pub mod provider;
pub mod rules;
pub mod run_grant;
pub mod secret_patterns;
pub mod spill;
pub mod validate;

pub use audit::{approval_kind, install_direct_ledger, record_direct, set_direct_run_source, ApprovalNote, AuditEvent, AuditLedger, ChainError, LedgerCheck, Sealer};
pub use canon::{canonical_json, hex_lower};
pub use config_text::{ConfigError, ConfigErrorKind};
pub use envelope::{ApprovalRecord, Artifact, ErrorCode, ToolError, ToolResult};
pub use explain::{explain, preview_text, shown_whole, ApprovalWhy, PREVIEW_HIDDEN, PREVIEW_MAX_CHARS};
pub use heads::{key_id_of, Head, HeadSigner, HeadsReport, HEAD_EVERY};
pub use proof::{verify_bundle, BundleReport, ProofLine, RunProof};
pub use hooks::{tool_pattern_matches, HookAnswer, HookCall, HookEvent, Hooks, PostAnswer};
pub use host::{call_digest, short_digest, AgentScope, ConsentView, UNATTENDED_MAY_CHANGE_ENV, DeniedView, DispatchObserver, Preview, RequestContext, ToolHost, ToolRequest, MAX_APPROVAL_WAIT_SECS, MAX_PERSON_SAID_CHARS, MAX_REFUSALS_IN_A_ROW};
pub use manifest::{is_valid_tool_name, Annotations, Decision, PermClass, ToolManifest};
pub use output::Redactor;
pub use preapproval::{Preapprovals, ProjectCheck};
pub use progress::{ProgressCall, ProgressEmit, ProgressPiece, ProgressSink, ProgressSource};
pub use spill::{FreeSpaceProbe, SpillConfig, SpillRef, SpillStore, SweepReport};
pub use policy::{edit_grant_lifts_taint, edit_grant_possible, is_egress, Caller, CapsMode, EditGrantFacts, Limits, Mode, PolicyContext, PolicyEngine, PolicyFile, Verdict};
pub use run_grant::{EditScope, GrantView, RunGrants};
pub use provider::{
    ApprovalOutcome, ApprovalRequest, ApprovalScope, Approver, Availability, ChangePreview, Classification, DenyAll, ExecCtx,
    ProviderFuture, ProviderOutput, ToolProvider,
};
pub use rules::{Effect as RuleEffect, PatternRule, RuleFile, RuleSet};
pub use validate::{validate_args, validate_args_noted, Validated};
