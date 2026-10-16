//! The contract of the API: the version a client can ask for, the stable error codes and the layer that puts them on every error (card OA10-02, tasks UX-04, AQ-17 and EX-18 of the
//! OpenCode audit).
//!
//! * **Errors.** An error body is `{ "error": "a sentence a person can read", "code": "not_found", "hint": "...", "retryable": true }`. `error` is what it always was; `code` is one
//!   of the fifteen words of [`ErrorCode`] and never changes (a client switches on it); `hint` is a fixed sentence on what to do (it never holds a path, a token or anything the
//!   request said); `retryable` is present, and true, for the codes where asking again can work. Most handlers say `api_error(status, message)`: the code is then the one of the
//!   status ([`ErrorCode::for_status`]). A handler that knows better says `api_error_code`. A handler that builds its own `{ "error": ... }` is covered by [`contract_layer`],
//!   which adds the code of the status to any error response whose body is a JSON object with a text `error` and no `code`. The envelope of `POST /api/v1/tools/call` is not one of
//!   these: its `error` is an object with the tool host's own codes (`documentation/ERROR_CODES.md` says both).
//! * **The version.** `GET /api/v1/version` is `{ core, contract, min_client }`; every response carries `x-aqua-contract: <contract>`. The contract number goes up when a field is
//!   removed or changes its meaning, never for a field that is added.
//! * **The schemas** of the bodies are in `contract_schemas` and, through `openapi.rs`, in `documentation/openapi.json`.

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

/// The version of this contract. Raised when a field is removed or changes its meaning; a field that is added does not raise it.
pub const CONTRACT_VERSION: u32 = 1;
/// The oldest contract a client may have been built for and still be served as it expects.
pub const MIN_CLIENT_CONTRACT: u32 = 1;
/// The header every response carries, with [`CONTRACT_VERSION`] as its value.
pub const CONTRACT_HEADER: &str = "x-aqua-contract";

/// The most bytes of an error body that [`contract_layer`] reads to add a code to it. An error body is a sentence; a longer one is passed on without a code (it never is).
const MAX_ERROR_BODY: usize = 256 * 1024;

/// What went wrong, in a word that does not change. The first fifteen of the contract (card OA10-02); a code is only ever added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// No token, or a wrong one (401).
    Unauthorized,
    /// The caller may not do this: the route needs the UI token and the request has the API token, or the folder, site or switch it names is not one Aqua may use (403).
    ForbiddenScope,
    /// The `Host` of the request is not this machine's loopback (403).
    HostNotAllowed,
    /// The `Origin` of the request is not one the app uses (403).
    OriginNotAllowed,
    /// Aqua is serving another request and this one waited too long for its turn (503). Ask again.
    Busy,
    /// Something this needs is not running or not ready yet: an engine, the tool host, the run database (503). Ask again later.
    NotReady,
    /// There is no such run, route, job, file or setting (404).
    NotFound,
    /// It cannot be done now: it is running, it was decided already, someone else is at it (409).
    Conflict,
    /// Too late: the question expired, the time to answer has passed (410).
    TooLate,
    /// A limit was reached: too many requests, too many runs, too long (429).
    LimitReached,
    /// The request is wrong: a field is missing or not allowed, a value is out of range, the body is too large (400, 413, 422).
    InvalidRequest,
    /// The language model or a speech engine is not available (503), or it failed while it was asked (502).
    ModelUnavailable,
    /// It took longer than it may (504). Ask again, or run it in the background.
    Timeout,
    /// The person or the caller stopped it (409 for a stopped task).
    Cancelled,
    /// Anything else: a fault in Aqua, not in the request (500 and the statuses nothing above fits).
    Internal,
}

impl ErrorCode {
    /// Every code, in the order `documentation/ERROR_CODES.md` lists them.
    pub const ALL: [ErrorCode; 15] = [
        ErrorCode::Unauthorized,
        ErrorCode::ForbiddenScope,
        ErrorCode::HostNotAllowed,
        ErrorCode::OriginNotAllowed,
        ErrorCode::Busy,
        ErrorCode::NotReady,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::TooLate,
        ErrorCode::LimitReached,
        ErrorCode::InvalidRequest,
        ErrorCode::ModelUnavailable,
        ErrorCode::Timeout,
        ErrorCode::Cancelled,
        ErrorCode::Internal,
    ];

    /// The word on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::ForbiddenScope => "forbidden_scope",
            ErrorCode::HostNotAllowed => "host_not_allowed",
            ErrorCode::OriginNotAllowed => "origin_not_allowed",
            ErrorCode::Busy => "busy",
            ErrorCode::NotReady => "not_ready",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::TooLate => "too_late",
            ErrorCode::LimitReached => "limit_reached",
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::ModelUnavailable => "model_unavailable",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Internal => "internal",
        }
    }

    /// The code a word on the wire stands for.
    pub fn parse(word: &str) -> Option<ErrorCode> {
        ErrorCode::ALL.into_iter().find(|c| c.as_str() == word)
    }

    /// Whether asking again, unchanged, can work: Aqua was busy, it was not ready yet, or the time ran out.
    pub const fn retryable(self) -> bool {
        matches!(self, ErrorCode::Busy | ErrorCode::NotReady | ErrorCode::Timeout)
    }

    /// What the code says, for the page that lists them.
    pub const fn meaning(self) -> &'static str {
        match self {
            ErrorCode::Unauthorized => "no token, or a wrong one",
            ErrorCode::ForbiddenScope => "the caller may not do this: the route needs the UI token, or what it names is not something Aqua may use",
            ErrorCode::HostNotAllowed => "the Host header is not this machine's loopback",
            ErrorCode::OriginNotAllowed => "the Origin header is not one the app uses",
            ErrorCode::Busy => "Aqua is serving another request and this one waited too long for its turn",
            ErrorCode::NotReady => "something the request needs is not running or not ready yet",
            ErrorCode::NotFound => "there is no such run, job, route or setting",
            ErrorCode::Conflict => "it cannot be done now: it is running, it was decided already, or another request is at it",
            ErrorCode::TooLate => "too late: the question expired or the time to answer has passed",
            ErrorCode::LimitReached => "a limit was reached: too many requests, runs or streams, or something too long",
            ErrorCode::InvalidRequest => "the request is wrong: a field is missing or not allowed, a value is out of range, or the body is too large",
            ErrorCode::ModelUnavailable => "the language model or a speech engine is not available, or failed",
            ErrorCode::Timeout => "it took longer than it may",
            ErrorCode::Cancelled => "the person or the caller stopped it",
            ErrorCode::Internal => "a fault in Aqua, not in the request",
        }
    }

    /// What to do about it, as a fixed sentence, for the codes where there is one to say: it holds nothing of the request, no path and no token.
    pub const fn hint(self) -> Option<&'static str> {
        match self {
            ErrorCode::Busy => Some("another request is being served: send this one again in a moment"),
            ErrorCode::NotReady => Some("what it needs is still starting or is off: look at GET /api/v1/status, then ask again"),
            ErrorCode::ModelUnavailable => Some("a model or engine is not loaded: run aqua-cli doctor to see which"),
            ErrorCode::Timeout => Some("ask again, or start a long task in the background (background: true)"),
            _ => None,
        }
    }

    /// The code of a status, for a handler that says only the status. The statuses the API sends are the ones of the page `documentation/UI_CONTRACT.md` lists; any other 4xx is a
    /// wrong request and any other status from 500 on is Aqua's own fault.
    pub fn for_status(status: StatusCode) -> ErrorCode {
        match status.as_u16() {
            401 => ErrorCode::Unauthorized,
            403 => ErrorCode::ForbiddenScope,
            404 => ErrorCode::NotFound,
            408 | 504 => ErrorCode::Timeout,
            409 => ErrorCode::Conflict,
            410 => ErrorCode::TooLate,
            429 => ErrorCode::LimitReached,
            499 => ErrorCode::Cancelled,
            503 => ErrorCode::NotReady,
            400..=499 => ErrorCode::InvalidRequest,
            _ => ErrorCode::Internal,
        }
    }
}

/// An error body: `{ error, code }`, with `hint` when there is one and `retryable: true` for a code where asking again can work. The `hint` is a fixed sentence; pass nothing the
/// request said.
pub fn error_body(code: ErrorCode, message: &str, hint: Option<&str>) -> Value {
    let mut body = json!({ "error": message, "code": code.as_str() });
    if let Some(hint) = hint.filter(|h| !h.is_empty()) {
        body["hint"] = json!(hint);
    }
    if code.retryable() {
        body["retryable"] = json!(true);
    }
    body
}

/// An error response with a code of the handler's choosing (card OA10-02). [`api_error`](super::server::api_error) is this with the code of the status.
pub fn api_error_code(status: StatusCode, code: ErrorCode, message: &str, hint: Option<&str>) -> Response {
    (status, Json(error_body(code, message, hint))).into_response()
}

/// The body with the code of `status` added, when it is a JSON object with a text `error` and no `code`; `None` when there is nothing to add (it has a code, it is not an error body of
/// this API, or it is not JSON).
fn with_code(status: StatusCode, bytes: &[u8]) -> Option<Vec<u8>> {
    let mut body: Value = serde_json::from_slice(bytes).ok()?;
    let map = body.as_object_mut()?;
    if !map.get("error").is_some_and(Value::is_string) || map.contains_key("code") {
        return None;
    }
    let code = ErrorCode::for_status(status);
    map.insert("code".into(), json!(code.as_str()));
    if code.retryable() && !map.contains_key("retryable") {
        map.insert("retryable".into(), json!(true));
    }
    serde_json::to_vec(&body).ok()
}

fn is_json(response: &Response) -> bool {
    response.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"))
}

/// The layer over the whole API (card OA10-02): puts `x-aqua-contract` on every response, and a `code` on every error body that has none, whichever handler made it. An error body
/// that has a code already, a body that is not JSON (the framework's own refusal of a request that is too large, say) and every answer below 400 are passed on as they are.
pub async fn contract_layer(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    response.headers_mut().insert(HeaderName::from_static(CONTRACT_HEADER), HeaderValue::from(CONTRACT_VERSION));
    let status = response.status();
    if status.as_u16() < 400 || !is_json(&response) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    match axum::body::to_bytes(body, MAX_ERROR_BODY).await {
        Ok(bytes) => match with_code(status, &bytes) {
            Some(coded) => {
                parts.headers.remove(header::CONTENT_LENGTH);
                Response::from_parts(parts, Body::from(coded))
            }
            None => Response::from_parts(parts, Body::from(bytes)),
        },
        // Longer than any error is: what it said is lost, and the client is told that and the status it had.
        Err(_) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            let code = ErrorCode::for_status(status);
            let body = serde_json::to_vec(&error_body(code, "the error was too long to show", None)).unwrap_or_default();
            Response::from_parts(parts, Body::from(body))
        }
    }
}

/// `GET /api/v1/version`: `{ core, contract, min_client }`.
pub fn version() -> Value {
    json!({ "core": env!("CARGO_PKG_VERSION"), "contract": CONTRACT_VERSION, "min_client": MIN_CLIENT_CONTRACT })
}

/// Which core this is and which version of this contract it speaks (`contract`), and the oldest contract of a client it still serves as that client expects (`min_client`). A client
/// whose own contract is below `min_client`, or above `contract`, should tell the person to update, not guess. Either token.
async fn version_endpoint() -> Json<Value> {
    Json(version())
}

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/api/v1/version", get(version_endpoint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    async fn read(response: Response) -> (StatusCode, Value, axum::http::HeaderMap) {
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        (parts.status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), parts.headers)
    }

    fn app() -> Router {
        Router::new()
            .route("/plain", get(|| async { (StatusCode::NOT_FOUND, Json(json!({ "error": "no such thing" }))) }))
            .route("/busy", get(|| async { (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "wait" }))) }))
            .route("/coded", get(|| async { api_error_code(StatusCode::CONFLICT, ErrorCode::Cancelled, "stopped", Some("start it again")) }))
            .route("/envelope", get(|| async { (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "ok": false, "error": { "code": "denied", "message": "no" } }))) }))
            .route("/text", get(|| async { (StatusCode::PAYLOAD_TOO_LARGE, "too large") }))
            .route("/fine", get(|| async { Json(json!({ "error": "this is a field of an answer, not an error" })) }))
            .route("/with_other_fields", get(|| async { (StatusCode::CONFLICT, Json(json!({ "error": "the job has ended already", "job": { "id": 3 } }))) }))
            .merge(routes())
            .layer(axum::middleware::from_fn(contract_layer))
    }

    async fn get_path(path: &str) -> (StatusCode, Value, axum::http::HeaderMap) {
        read(app().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap()).await
    }

    #[test]
    fn there_are_fifteen_codes_with_distinct_words_that_parse_back() {
        let words: std::collections::BTreeSet<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(words.len(), 15, "two codes with one word");
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
            assert!(code.as_str().chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{}", code.as_str());
            assert!(code.meaning().len() > 10, "{} has no meaning", code.as_str());
        }
        assert_eq!(ErrorCode::parse("nonsense"), None);
        // Asking again can work for these three and no other (the contract of the card).
        let retryable: Vec<&str> = ErrorCode::ALL.iter().filter(|c| c.retryable()).map(|c| c.as_str()).collect();
        assert_eq!(retryable, ["busy", "not_ready", "timeout"]);
    }

    #[test]
    fn a_hint_is_a_fixed_sentence_for_the_codes_that_have_one() {
        for code in ErrorCode::ALL {
            if let Some(hint) = code.hint() {
                assert!(hint.len() > 20 && !hint.contains(":\\") && !hint.to_lowercase().contains("bearer"), "{hint}");
            }
        }
        // Every code where asking again can work says what to do.
        assert!(ErrorCode::ALL.iter().filter(|c| c.retryable()).all(|c| c.hint().is_some()));
    }

    #[test]
    fn every_status_the_api_sends_has_the_code_the_page_of_codes_says() {
        let code = |n: u16| ErrorCode::for_status(StatusCode::from_u16(n).unwrap()).as_str();
        assert_eq!(
            [400, 401, 403, 404, 409, 410, 413, 422, 429, 500, 502, 503, 504].map(code),
            [
                "invalid_request",
                "unauthorized",
                "forbidden_scope",
                "not_found",
                "conflict",
                "too_late",
                "invalid_request",
                "invalid_request",
                "limit_reached",
                "internal",
                "internal",
                "not_ready",
                "timeout"
            ]
        );
    }

    #[test]
    fn an_error_body_has_the_error_the_code_and_only_what_it_needs_besides() {
        assert_eq!(error_body(ErrorCode::NotFound, "no run called x", None), json!({ "error": "no run called x", "code": "not_found" }));
        assert_eq!(error_body(ErrorCode::Busy, "wait", Some("send it again")), json!({ "error": "wait", "code": "busy", "hint": "send it again", "retryable": true }));
        assert_eq!(error_body(ErrorCode::Conflict, "m", Some("")), json!({ "error": "m", "code": "conflict" }), "an empty hint is no hint");
    }

    #[tokio::test]
    async fn the_layer_adds_the_code_of_the_status_to_an_error_body_that_has_none_and_keeps_the_other_fields() {
        let (status, body, headers) = get_path("/plain").await;
        assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({ "error": "no such thing", "code": "not_found" })));
        assert_eq!(headers.get(CONTRACT_HEADER).unwrap(), "1");
        let (_, body, _) = get_path("/busy").await;
        assert_eq!(body, json!({ "error": "wait", "code": "not_ready", "retryable": true }));
        let (status, body, _) = get_path("/with_other_fields").await;
        assert_eq!((status, body), (StatusCode::CONFLICT, json!({ "error": "the job has ended already", "job": { "id": 3 }, "code": "conflict" })));
    }

    #[tokio::test]
    async fn the_layer_leaves_alone_a_code_that_is_there_an_envelope_with_its_own_codes_text_and_every_answer_that_is_not_an_error() {
        let (_, body, _) = get_path("/coded").await;
        assert_eq!(body, json!({ "error": "stopped", "code": "cancelled", "hint": "start it again" }));
        let (status, body, _) = get_path("/envelope").await;
        assert_eq!((status, body), (StatusCode::UNPROCESSABLE_ENTITY, json!({ "ok": false, "error": { "code": "denied", "message": "no" } })), "the tool envelope has its own error object");
        let (status, _, headers) = get_path("/text").await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(headers.get(CONTRACT_HEADER).unwrap(), "1", "the header is on every answer");
        let (status, body, _) = get_path("/fine").await;
        assert_eq!((status, body), (StatusCode::OK, json!({ "error": "this is a field of an answer, not an error" })));
    }

    #[tokio::test]
    async fn the_version_route_says_the_core_the_contract_and_the_oldest_client() {
        let (status, body, headers) = get_path("/api/v1/version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "core": env!("CARGO_PKG_VERSION"), "contract": CONTRACT_VERSION, "min_client": MIN_CLIENT_CONTRACT }));
        assert_eq!(headers.get(CONTRACT_HEADER).unwrap().to_str().unwrap(), CONTRACT_VERSION.to_string());
        assert!(MIN_CLIENT_CONTRACT <= CONTRACT_VERSION);
    }

    #[test]
    fn a_hint_and_a_message_of_this_file_hold_no_path_and_no_token() {
        // The codes' own words are the only text this file adds to an error; none names a path or a credential.
        for code in ErrorCode::ALL {
            let text = format!("{} {}", code.as_str(), code.meaning());
            assert!(!text.contains(":\\") && !text.contains('/') && !text.to_lowercase().contains("token="), "{text}");
        }
    }
}
