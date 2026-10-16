//! The answers that say a request failed (card OA3-06, moved out of `server.rs`): `api_error`, a turn's failure in words that show no folder and no stderr (`describe_pipeline_fault`, `pipeline_failure`) and an engine that failed upstream (`upstream_error`).

use super::*;

/// An error answer: `{ "error": message, "code": ... }`, with the code of the status (`ErrorCode::for_status`). A handler that knows a better code says [`api_error_code`] (card OA10-02).
pub fn api_error(status: axum::http::StatusCode, message: &str) -> axum::response::Response {
    api_error_code(status, super::super::contract::ErrorCode::for_status(status), message, None)
}

/// Turn an internal failure into a status and a message that is safe to show: no folder paths, no stderr dumps.
pub fn describe_pipeline_error(raw: &str) -> (axum::http::StatusCode, String) {
    let (status, _, message) = describe_pipeline_fault(raw);
    (status, message)
}

/// [`describe_pipeline_error`], also saying which code of the contract the failure is (card OA10-02). The failures of a turn reach this as text (the work of a turn runs on a thread and
/// ends in a `String`), so the code is found from the words, as the status always was; a failure that is made here with a name (a stopped task, a turn that timed out) says it.
pub fn describe_pipeline_fault(raw: &str) -> (axum::http::StatusCode, super::super::contract::ErrorCode, String) {
    use super::super::contract::ErrorCode;
    use axum::http::StatusCode;
    if raw.trim_start().to_lowercase().starts_with("stopped") {
        return (StatusCode::CONFLICT, ErrorCode::Cancelled, "The task was stopped.".to_string());
    }
    let first = raw.lines().next().unwrap_or("").trim();
    let redacted: String = first
        .split(' ')
        .map(|w| {
            let bare = w.trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '(' || c == ')' || c == ',');
            if bare.contains(":\\") || bare.contains(":/") && bare.len() > 3 && bare.as_bytes().get(1) == Some(&b':') || bare.starts_with("\\\\") {
                "<path>"
            } else {
                w
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let message: String = redacted.chars().take(300).collect();
    let lower = redacted.to_lowercase();
    let (status, code) = if lower.contains("timed out") || lower.contains("timeout") {
        (StatusCode::GATEWAY_TIMEOUT, ErrorCode::Timeout)
    } else if lower.contains("not available") || lower.contains("is missing") || lower.contains("not found") || lower.contains("could not run") {
        // A model or an engine that is not there is `model_unavailable`; any other thing that is not ready (the tool host, a database) is `not_ready`.
        let model = ["model", "llama", "whisper", "kokoro", "speech", "engine"].iter().any(|w| lower.contains(w));
        (StatusCode::SERVICE_UNAVAILABLE, if model { ErrorCode::ModelUnavailable } else { ErrorCode::NotReady })
    } else if lower.contains("empty") || lower.contains("invalid") || lower.contains("decode") {
        (StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest)
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal)
    };
    (status, code, if message.is_empty() { "the request failed".to_string() } else { message })
}

pub(in crate::api_gateway) fn pipeline_failure(raw: &str) -> axum::response::Response {
    tracing::warn!("pipeline request failed: {raw}");
    let (status, code, message) = describe_pipeline_fault(raw);
    api_error_code(status, code, &message, code.hint())
}

/// Engine failures are reported as 502 with the real error, never as a plausible-looking result.
pub(super) fn upstream_error(engine: &str, e: anyhow::Error) -> Response {
    // The full chain (paths, stderr, a traceback) goes to the log; the caller gets the first line with folders hidden.
    let raw = format!("{e:#}");
    tracing::warn!("{engine} request failed: {raw}");
    let (_, message) = describe_pipeline_error(&raw);
    api_error_code(axum::http::StatusCode::BAD_GATEWAY, super::super::contract::ErrorCode::ModelUnavailable, &format!("{engine}: {message}"), super::super::contract::ErrorCode::ModelUnavailable.hint())
}
