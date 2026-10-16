//! What is put around a request, and the limits it carries (card OA3-06, moved out of `server.rs`): the largest audio body, the deadline after which a request is answered 504 (streams are never cut), and the span that ties what the core logs to the request that made it.

use super::*;

/// Largest audio request body (base64 JSON). The default of 2 MB cut off anything over about 47 s of 16 kHz mono;
/// 32 MB is about 12 minutes, and a request over it gets a clear 413 from the framework.
pub(in crate::api_gateway) const AUDIO_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// Longest a request may take before the gateway answers 504 and drops it (`AQUA_REQUEST_MAX_SECS`, default 900).
/// Streams (server-sent events, the WebSocket, token streaming) are open for as long as they are used and are not
/// cut. This is a backstop for the routes that have no deadline of their own: nothing may hold a connection (and the
/// work behind it) forever.
fn request_limit() -> std::time::Duration {
    std::time::Duration::from_secs(std::env::var("AQUA_REQUEST_MAX_SECS").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(900).clamp(5, 86_400))
}

pub(super) fn is_stream_path(path: &str) -> bool {
    path == "/ws" || path.ends_with("/ws") || path.contains("/stream") || path.contains("_stream")
}

pub(super) async fn request_deadline(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    deadline_for(req, next, request_limit()).await
}

/// The number of the next request (the first in this process is 1).
static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The id of a request: unique to the process (the number) and to its start (the time in hex), so that two runs of the core do not give one id twice in one log file.
pub(crate) fn request_id(n: u64) -> String {
    static STARTED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let started = *STARTED.get_or_init(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    format!("rq_{started:x}_{n}")
}

/// Writes everything the core logs while it handles a request inside the span `request{request_id, method, path}` (card OA3-04), so that a line can be tied to the request that made it.
/// The probe of `/api/v1/health` (the start-up scripts and the doctor ask it all the time) has none. The path only: the query of a request can hold what the person asked, and is never logged.
/// One line at the end, at `debug`: the status and how long it took.
pub(super) async fn request_span(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use tracing::Instrument;
    if req.uri().path() == "/api/v1/health" {
        return next.run(req).await;
    }
    let id = request_id(NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let path: String = req.uri().path().chars().take(200).map(|c| if c.is_control() { '_' } else { c }).collect();
    let span = tracing::info_span!("request", request_id = %id, method = %req.method(), path = %path);
    let started = std::time::Instant::now();
    let response = next.run(req).instrument(span.clone()).await;
    span.in_scope(|| tracing::debug!(status = response.status().as_u16(), ms = started.elapsed().as_millis() as u64, "request ended"));
    response
}

pub(super) async fn deadline_for(req: axum::extract::Request, next: axum::middleware::Next, limit: std::time::Duration) -> axum::response::Response {
    if is_stream_path(req.uri().path()) {
        return next.run(req).await;
    }
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(response) => response,
        Err(_) => api_error_code(axum::http::StatusCode::GATEWAY_TIMEOUT, super::super::contract::ErrorCode::Timeout, &format!("the request took longer than {} seconds and was stopped", limit.as_secs()), super::super::contract::ErrorCode::Timeout.hint()),
    }
}
