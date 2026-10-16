//! The one result shape every tool returns, and the one error vocabulary they all use.

use serde::{Deserialize, Serialize};
use serde_json::Value;

macro_rules! error_codes {
    ($( $variant:ident => $text:literal, retryable: $retry:literal, stops_run: $stop:literal; )*) => {
        /// Error codes. The list is closed: tools use these and nothing else.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum ErrorCode { $( #[serde(rename = $text)] $variant, )* }

        impl ErrorCode {
            pub const ALL: &'static [ErrorCode] = &[ $( ErrorCode::$variant, )* ];
            pub fn as_str(self) -> &'static str { match self { $( ErrorCode::$variant => $text, )* } }
            /// Worth retrying as is (after a delay), when the tool is safe to repeat.
            pub fn retryable(self) -> bool { match self { $( ErrorCode::$variant => $retry, )* } }
            /// The agent must stop the run: retrying another way would go around a decision.
            pub fn stops_run(self) -> bool { match self { $( ErrorCode::$variant => $stop, )* } }
        }
    };
}

error_codes! {
    InvalidArguments => "INVALID_ARGUMENTS", retryable: false, stops_run: false;
    NotFound => "NOT_FOUND", retryable: false, stops_run: false;
    AlreadyExists => "ALREADY_EXISTS", retryable: false, stops_run: false;
    Conflict => "CONFLICT", retryable: true, stops_run: false;
    PermissionDenied => "PERMISSION_DENIED", retryable: false, stops_run: true;
    OsPermissionDenied => "OS_PERMISSION_DENIED", retryable: false, stops_run: false;
    ApprovalRequired => "APPROVAL_REQUIRED", retryable: false, stops_run: true;
    ApprovalDenied => "APPROVAL_DENIED", retryable: false, stops_run: true;
    ApprovalTimeout => "APPROVAL_TIMEOUT", retryable: false, stops_run: true;
    Timeout => "TIMEOUT", retryable: true, stops_run: false;
    Cancelled => "CANCELLED", retryable: false, stops_run: true;
    ResourceLimit => "RESOURCE_LIMIT", retryable: false, stops_run: false;
    RateLimited => "RATE_LIMITED", retryable: true, stops_run: false;
    Unavailable => "UNAVAILABLE", retryable: true, stops_run: false;
    AuthRequired => "AUTH_REQUIRED", retryable: false, stops_run: false;
    AuthFailed => "AUTH_FAILED", retryable: false, stops_run: false;
    NetworkBlocked => "NETWORK_BLOCKED", retryable: false, stops_run: true;
    SandboxViolation => "SANDBOX_VIOLATION", retryable: false, stops_run: true;
    UnsupportedPlatform => "UNSUPPORTED_PLATFORM", retryable: false, stops_run: false;
    SecretBlocked => "SECRET_BLOCKED", retryable: false, stops_run: true;
    BackendError => "BACKEND_ERROR", retryable: false, stops_run: false;
    HostKeyUnknown => "HOST_KEY_UNKNOWN", retryable: false, stops_run: true;
    HostKeyMismatch => "HOST_KEY_MISMATCH", retryable: false, stops_run: true;
    DirtyWorktree => "DIRTY_WORKTREE", retryable: false, stops_run: false;
    MergeConflict => "MERGE_CONFLICT", retryable: false, stops_run: false;
    NonFastForward => "NON_FAST_FORWARD", retryable: false, stops_run: false;
    ProtectedRef => "PROTECTED_REF", retryable: false, stops_run: true;
    QueryRejected => "QUERY_REJECTED", retryable: false, stops_run: false;
    ProdProtected => "PROD_PROTECTED", retryable: false, stops_run: true;
    TxnAborted => "TXN_ABORTED", retryable: true, stops_run: false;
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default)]
    pub details: Value,
}

impl ToolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), retryable: code.retryable(), details: Value::Null }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    /// Says that the message holds text somebody else wrote (a line of a file, say), though the tool is one whose results are not marked untrusted. The host then shows the failure to the
    /// model inside the same fence as the failure of an untrusted tool and counts the run as having read text from outside (card OA6-04, follow-up of the security review).
    pub fn holding_outside_text(mut self) -> Self {
        match &mut self.details {
            Value::Object(fields) => {
                fields.insert(OUTSIDE_TEXT.into(), Value::Bool(true));
            }
            other => *other = serde_json::json!({ OUTSIDE_TEXT: true }),
        }
        self
    }

    /// Was this error marked by [`ToolError::holding_outside_text`]?
    pub fn holds_outside_text(&self) -> bool {
        self.details.get(OUTSIDE_TEXT).and_then(Value::as_bool) == Some(true)
    }

    /// Says, as well as [`ToolError::holding_outside_text`], that the text the message holds is a line of a file inside a trusted folder of the project and nothing else (card OA6-05, ED-10):
    /// the run is counted as having read it, as before, but not as having read text from outside the project, which is what would end a run's edit grant. The host honours it only for a
    /// tool whose results are not marked untrusted (a file tool's failure that quotes the file it was asked to edit); a failure of any other tool is outside text whatever its details say.
    pub fn holding_local_text(mut self) -> Self {
        if let Value::Object(fields) = &mut self.details {
            fields.insert(LOCAL_TEXT.into(), Value::Bool(true));
        }
        self
    }

    /// Was this error marked by [`ToolError::holding_local_text`] (together with `holding_outside_text`)?
    pub fn holds_local_text(&self) -> bool {
        self.holds_outside_text() && self.details.get(LOCAL_TEXT).and_then(Value::as_bool) == Some(true)
    }

    /// Says what the person typed with their "no" (card OA10-03, AG-11). `said` is their own words, already masked, on one line and cut by the host. It is kept apart from the message
    /// so that the loop can tell a refusal that comes with an instruction from a plain one, and show the words as data from the person and not as a rule.
    pub fn with_person_said(mut self, said: &str) -> Self {
        match &mut self.details {
            Value::Object(fields) => {
                fields.insert(PERSON_SAID.into(), Value::String(said.to_string()));
            }
            other => *other = serde_json::json!({ PERSON_SAID: said }),
        }
        self
    }

    /// What the person typed with their "no", if they typed anything (see [`ToolError::with_person_said`]).
    pub fn person_said(&self) -> Option<&str> {
        self.details.get(PERSON_SAID).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
    }
}

/// The key of `ToolError::details` that marks an error whose message holds text somebody else wrote.
const OUTSIDE_TEXT: &str = "outside_text";

/// The key of `ToolError::details` that says the outside text is a line of a file of the project (see [`ToolError::holding_local_text`]).
const LOCAL_TEXT: &str = "local_text";

/// The key of `ToolError::details` that holds the words the person typed with their "no".
const PERSON_SAID: &str = "person_said";

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// `file`, `screenshot`, `log`, `patch` or `report`.
    pub kind: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// How a call was allowed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub id: String,
    /// `auto`, `approved`, `approved_session`, `approved_always`, `preapproved` (the person allowed this run's project checks ahead of time, card OA10-05), `approved_run` (the person
    /// approved this edit and later edits in its folder until the run ends, card OA6-05), `run_granted` (an edit that went through on such a grant, without a question), `denied` or `timeout`.
    pub decision: String,
    /// `policy`, `ui`, `voice`, `preapproval` or `run_grant`.
    pub by: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub ok: bool,
    pub tool: String,
    pub request_id: String,
    pub data: Value,
    /// What the model reads: redacted, labelled when untrusted, capped.
    pub text: String,
    pub truncated: bool,
    pub artifacts: Vec<Artifact>,
    pub warnings: Vec<String>,
    pub approval: Option<ApprovalRecord>,
    pub duration_ms: u64,
    pub error: Option<ToolError>,
}

impl ToolResult {
    pub fn failure(tool: &str, request_id: &str, error: ToolError, duration_ms: u64) -> Self {
        let text = format!("ERROR [{}] {}: {}", error.code, tool, error.message);
        Self {
            ok: false,
            tool: tool.to_string(),
            request_id: request_id.to_string(),
            data: Value::Null,
            text,
            truncated: false,
            artifacts: Vec::new(),
            warnings: Vec::new(),
            approval: None,
            duration_ms,
            error: Some(error),
        }
    }

    /// The text an agent loop sees. Failures start `ERROR [CODE] tool:` and the loop keys on that.
    pub fn agent_text(&self) -> &str {
        &self.text
    }

    pub fn stops_run(&self) -> bool {
        self.error.as_ref().is_some_and(|e| e.code.stops_run())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_serialize_as_their_wire_names() {
        assert_eq!(serde_json::to_string(&ErrorCode::HostKeyMismatch).unwrap(), "\"HOST_KEY_MISMATCH\"");
        for c in ErrorCode::ALL {
            let json = serde_json::to_string(c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
            let back: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, *c);
        }
    }

    #[test]
    fn the_code_list_has_no_duplicates() {
        let mut names: Vec<_> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len());
        assert_eq!(n, 30);
    }

    #[test]
    fn failure_text_has_the_prefix_the_agent_loop_keys_on() {
        let r = ToolResult::failure("fs.read", "req_1", ToolError::new(ErrorCode::NotFound, "no such file"), 3);
        assert_eq!(r.agent_text(), "ERROR [NOT_FOUND] fs.read: no such file");
        assert!(!r.ok && r.data.is_null() && !r.stops_run());
        let denied = ToolResult::failure("term.run", "req_2", ToolError::new(ErrorCode::PermissionDenied, "x"), 0);
        assert!(denied.stops_run());
    }
}
