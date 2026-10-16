//! Local API protection for the gateway.
//!
//! The gateway can drive Windows-MCP (PowerShell) and the browser, so "bound to 127.0.0.1" is not
//! enough: any web page open in the user's browser can reach localhost. Every request must:
//! - carry a `Host` of this machine's loopback (blocks DNS-rebinding),
//! - carry no `Origin`, or an allow-listed one (blocks cross-site fetches),
//! - present `Authorization: Bearer <token>` (except `GET /api/v1/health`).
//!
//! The token is random per install, stored in `data/.aqua_api_token` (git-ignored), or taken from
//! `AQUA_API_TOKEN`. The Vite dev proxy injects it, so browser code never sees it.

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::Response,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use super::contract::{api_error_code, ErrorCode};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{info, warn};

pub const TOKEN_ENV: &str = "AQUA_API_TOKEN";
pub const TOKEN_FILE: &str = ".aqua_api_token";
/// A second token, held only by the UI. Only it can answer approval prompts or change policy, so
/// a caller that has the API token (a script, a tool run by the agent) cannot approve its own action.
pub const UI_TOKEN_ENV: &str = "AQUA_UI_TOKEN";
pub const UI_TOKEN_FILE: &str = ".aqua_ui_token";
/// Comma-separated extra origins, e.g. `http://127.0.0.1:5173`.
pub const EXTRA_ORIGINS_ENV: &str = "AQUA_ALLOWED_ORIGINS";

const MIN_TOKEN_LEN: usize = 32;

pub const DEFAULT_ALLOWED_ORIGINS: &[&str] = &[
    "http://127.0.0.1:1420",
    "http://localhost:1420",
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
];

/// What a request's token allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The UI token: everything, including approving and changing policy.
    Ui,
    /// The API token: everything except approving and changing policy.
    Api,
}

/// Whether the request showed a token that was accepted (either of the two). The one route that answers without a token, `GET /api/v1/health`, tells a caller that showed none only
/// that Aqua is up and which version it is (card OA1-04, SE-19); the rest of its answer is the status, which is for the callers that hold a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenShown(pub bool);

/// A response for handlers that only the UI may serve.
pub fn ui_scope_required(scope: Scope) -> Option<Response> {
    (scope != Scope::Ui).then(|| {
        reject(
            StatusCode::FORBIDDEN,
            ErrorCode::ForbiddenScope,
            "this action needs the UI token: approvals and policy changes cannot be made with the API token",
            Some("this is the app's to do: use the UI, or ask for it in the app"),
        )
    })
}

#[derive(Clone)]
pub struct ApiAuth {
    ui_token: Arc<str>,
    token: Arc<str>,
    port: u16,
    origins: Arc<[String]>,
}

impl ApiAuth {
    pub fn new(token: String, port: u16) -> Self {
        Self {
            ui_token: "".into(),
            token: token.into(),
            port,
            origins: allowed_origins().into(),
        }
    }

    /// Set the UI token. Without one, nothing has the UI scope and approvals cannot be answered. A UI token that is the API token is not set (card OA1-04, SE-18): the UI scope wins
    /// in `scope_of`, so every holder of the API token would have had it, approvals and policy changes included. The loaders refuse such a pair before it gets here ([`check_distinct`]);
    /// this is the net below them, and it fails closed.
    pub fn with_ui_token(mut self, ui_token: String) -> Self {
        if constant_time_eq(ui_token.as_bytes(), self.token.as_bytes()) {
            warn!("the UI token is the same as the API token: no token has the UI scope until they differ");
            return self;
        }
        self.ui_token = ui_token.into();
        self
    }

    fn scope_of(&self, headers: &HeaderMap) -> Option<Scope> {
        if !self.ui_token.is_empty() && bearer_matches(headers, &self.ui_token) {
            Some(Scope::Ui)
        } else if bearer_matches(headers, &self.token) {
            Some(Scope::Api)
        } else {
            None
        }
    }

    /// CORS for the allow-listed UI origins only. Apply *outside* `require_local_auth`.
    pub fn cors_layer(&self) -> CorsLayer {
        let origins: Vec<HeaderValue> = self
            .origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o).ok())
            .collect();
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
            .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
            // Every request that carries the token is asked about first (a "preflight"), and the browser forgets the answer after
            // 5 seconds unless told otherwise, so the page's status polling doubled its own requests. An hour is well under the
            // 2-hour ceiling Chromium applies, and the allowed origins do not change while the core runs.
            .max_age(std::time::Duration::from_secs(3600))
            // A page of the app may read which contract an answer follows (card OA10-02).
            .expose_headers([header::HeaderName::from_static(super::contract::CONTRACT_HEADER)])
    }
}

pub fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKEN_FILE)
}

/// `AQUA_API_TOKEN` → existing token file → freshly generated token written to the file.
pub fn load_or_create_token(data_dir: &Path) -> Result<String> {
    load_or_create(data_dir, TOKEN_ENV, TOKEN_FILE)
}

/// The UI token: `AQUA_UI_TOKEN` → `data/.aqua_ui_token` → freshly generated. It is refused when it is the API token (the one both would then be): the UI scope can approve actions and
/// change policy, and the API token must not be able to.
pub fn load_or_create_ui_token(data_dir: &Path) -> Result<String> {
    let ui = load_or_create(data_dir, UI_TOKEN_ENV, UI_TOKEN_FILE)?;
    if let Some(api) = existing(data_dir, TOKEN_ENV, TOKEN_FILE) {
        check_distinct(&api, &ui)?;
    }
    Ok(ui)
}

/// An error when the two tokens are the same. Equal tokens make the API token a UI token (card OA1-04, SE-18).
pub fn check_distinct(api: &str, ui: &str) -> Result<()> {
    if constant_time_eq(api.as_bytes(), ui.as_bytes()) {
        return Err(anyhow!(
            "the UI token ({UI_TOKEN_ENV} or data/{UI_TOKEN_FILE}) is the same as the API token ({TOKEN_ENV} or data/{TOKEN_FILE}): the API token would be able to approve actions and change policy. Give them different values, or delete one of the files to have it made again"
        ));
    }
    Ok(())
}

/// The token that is there already (the variable, else a well-formed file), without making one.
fn existing(data_dir: &Path, env_name: &str, file: &str) -> Option<String> {
    if let Ok(t) = std::env::var(env_name) {
        let t = t.trim();
        if t.len() >= MIN_TOKEN_LEN {
            return Some(t.to_string());
        }
    }
    let text = std::fs::read_to_string(data_dir.join(file)).ok()?;
    let t = text.trim();
    (t.len() >= MIN_TOKEN_LEN && t.chars().all(|c| c.is_ascii_hexdigit())).then(|| t.to_string())
}

fn load_or_create(data_dir: &Path, env_name: &str, file: &str) -> Result<String> {
    if let Ok(t) = std::env::var(env_name) {
        let t = t.trim();
        if t.len() >= MIN_TOKEN_LEN {
            return Ok(t.to_string());
        }
        warn!("{env_name} is shorter than {MIN_TOKEN_LEN} chars — ignoring it");
    }

    let path = data_dir.join(file);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let t = existing.trim();
        if t.len() >= MIN_TOKEN_LEN && t.chars().all(|c| c.is_ascii_hexdigit()) {
            restrict_to_current_user(&path);
            return Ok(t.to_string());
        }
        warn!("{} is malformed — regenerating", path.display());
    }

    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("OS RNG unavailable: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("create {}", data_dir.display()))?;
    write_token_file(&path, &token).with_context(|| format!("write {}", path.display()))?;
    restrict_to_current_user(&path);
    info!("Generated new API token at {}", path.display());
    Ok(token)
}

/// Remove inherited permissions from the token file so only the current user can read it.
/// Best effort: a failure is logged, not fatal.
///
/// The file also gets a mandatory integrity label (Medium, no read up), so that a program of Low integrity, or in an AppContainer, cannot read it either (card R6-10,
/// T6-10.2). The person's own programs and Aqua are Medium and are not affected; a program the person approved without `isolated` is Medium too and still can.
pub fn restrict_to_current_user(path: &Path) {
    crate::protected_file::protect(path);
    #[cfg(windows)]
    {
        // The account by its SID and the program by its full path (card OA1-04, SE-18): the name in `%USERNAME%` is whatever the environment says, and `icacls` by name is the first one found.
        let sid = match aqua_native_tools::sandbox::current_user_sid() {
            Ok(sid) => sid,
            Err(e) => {
                warn!("could not restrict {} to this account: {e}", path.display());
                return;
            }
        };
        let out = icacls(path, &["/inheritance:r", "/grant:r", &format!("*{sid}:(R,W)")]);
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => warn!("icacls on {} failed: {}", path.display(), String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => warn!("could not run icacls on {}: {e}", path.display()),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Runs the system's `icacls.exe` (by full path) on `path` with `args`, with no window and no input.
#[cfg(windows)]
fn icacls(path: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(aqua_native_tools::sandbox::icacls_exe())
        .arg(path)
        .args(args)
        .stdin(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
}

/// Writes a new token file that only this account can open from its first byte (`sandbox::write_private_file`: it is created with its access list already set, so the folder's
/// inherited list never applies to it). An old file that is not a token is removed first. If that cannot be done the file is written the old way and [`restrict_to_current_user`]
/// narrows it afterwards: a token that cannot be made private at once is still made, as before (card OA1-04, SE-18).
fn write_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    match aqua_native_tools::sandbox::write_private_file(path, token.as_bytes()) {
        Ok(()) => Ok(()),
        Err(e) => {
            warn!("could not make {} private from the start ({e}); writing it the plain way and restricting it after", path.display());
            let _ = std::fs::remove_file(path);
            std::fs::write(path, token)
        }
    }
}

/// Limits a whole folder (the data folder) to this account: nothing inherited, full access for this account, and the same for what is in it (card OA1-04, SE-18; A6-51 of the sixth
/// audit: only the two token files were restricted, and the rest of `data` inherited what the drive gives every local account). **Not called by Aqua**: a side effect is that another
/// account that used to read the folder (a backup service, a second user) loses access, so it is the person's to switch on. On other systems the mode is 0700.
pub fn harden_data_dir(dir: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let sid = aqua_native_tools::sandbox::current_user_sid()?;
        let out = icacls(dir, &["/inheritance:r", "/grant:r", &format!("*{sid}:(OI)(CI)F")]).map_err(|e| format!("could not run icacls on {}: {e}", dir.display()))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("icacls on {} failed: {}", dir.display(), String::from_utf8_lossy(&out.stderr).trim()))
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("could not limit {}: {e}", dir.display()))
    }
}

fn allowed_origins() -> Vec<String> {
    let mut v: Vec<String> = DEFAULT_ALLOWED_ORIGINS.iter().map(|s| s.to_string()).collect();
    if let Ok(extra) = std::env::var(EXTRA_ORIGINS_ENV) {
        v.extend(
            extra
                .split(',')
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty()),
        );
    }
    v
}

fn host_allowed(headers: &HeaderMap, port: u16) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|h| host == *h || host == format!("{h}:{port}"))
}

fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|got| constant_time_eq(got.trim().as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A refusal of the gate: the sentence it always said, with the code of the contract (card OA10-02). The hints are fixed sentences: nothing of the request, and never a token.
fn reject(status: StatusCode, code: ErrorCode, msg: &str, hint: Option<&str>) -> Response {
    api_error_code(status, code, msg, hint)
}

pub async fn require_local_auth(
    State(auth): State<ApiAuth>,
    mut req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();

    if !host_allowed(headers, auth.port) {
        return reject(StatusCode::FORBIDDEN, ErrorCode::HostNotAllowed, "host not allowed", None);
    }

    if let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        if !auth.origins.iter().any(|o| o == origin) {
            warn!("Rejected request from origin {origin}");
            return reject(StatusCode::FORBIDDEN, ErrorCode::OriginNotAllowed, "origin not allowed", None);
        }
    }

    let public = req.method() == Method::GET && req.uri().path() == "/api/v1/health";
    let scope = auth.scope_of(headers);
    if !public && req.method() != Method::OPTIONS && scope.is_none() {
        return reject(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "missing or invalid API token (see data/.aqua_api_token)",
            Some("send the header Authorization: Bearer <token>"),
        );
    }
    // Public and preflight requests carry no token and get the lower scope.
    req.extensions_mut().insert(TokenShown(scope.is_some()));
    req.extensions_mut().insert(scope.unwrap_or(Scope::Api));

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn host_must_be_loopback_on_our_port() {
        assert!(host_allowed(&headers(&[(header::HOST, "127.0.0.1:8767")]), 8767));
        assert!(host_allowed(&headers(&[(header::HOST, "localhost:8767")]), 8767));
        assert!(!host_allowed(&headers(&[(header::HOST, "evil.example:8767")]), 8767));
        assert!(!host_allowed(&headers(&[(header::HOST, "127.0.0.1:9999")]), 8767));
        assert!(!host_allowed(&HeaderMap::new(), 8767));
    }

    #[test]
    fn the_ui_token_has_the_ui_scope_and_the_api_token_does_not() {
        let auth = ApiAuth::new("a".repeat(64), 8767).with_ui_token("b".repeat(64));
        let bearer = |t: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {t}")).unwrap());
            h
        };
        assert_eq!(auth.scope_of(&bearer(&"b".repeat(64))), Some(Scope::Ui));
        assert_eq!(auth.scope_of(&bearer(&"a".repeat(64))), Some(Scope::Api));
        assert_eq!(auth.scope_of(&bearer("wrong")), None);
        assert_eq!(auth.scope_of(&HeaderMap::new()), None);
        assert!(ui_scope_required(Scope::Api).is_some());
        assert!(ui_scope_required(Scope::Ui).is_none());
    }

    #[test]
    fn without_a_ui_token_nothing_has_the_ui_scope() {
        let auth = ApiAuth::new("a".repeat(64), 8767);
        let mut h = HeaderMap::new();
        // An empty bearer must not match an empty UI token.
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(auth.scope_of(&h), None);
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", "a".repeat(64))).unwrap());
        assert_eq!(auth.scope_of(&h), Some(Scope::Api));
    }

    #[test]
    fn the_ui_token_file_is_separate_from_the_api_token_file() {
        let dir = std::env::temp_dir().join(format!("aqua_ui_token_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var(TOKEN_ENV);
        std::env::remove_var(UI_TOKEN_ENV);
        let api = load_or_create_token(&dir).unwrap();
        let ui = load_or_create_ui_token(&dir).unwrap();
        assert_ne!(api, ui);
        assert!(dir.join(TOKEN_FILE).exists() && dir.join(UI_TOKEN_FILE).exists());
        assert_eq!(load_or_create_ui_token(&dir).unwrap(), ui, "stable across restarts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bearer_token_is_checked_exactly() {
        let tok = "a".repeat(64);
        let ok = headers(&[(header::AUTHORIZATION, &format!("Bearer {tok}"))]);
        assert!(bearer_matches(&ok, &tok));
        let bad = headers(&[(header::AUTHORIZATION, "Bearer aqua_admin_secure_token_xyz")]);
        assert!(!bearer_matches(&bad, &tok));
        assert!(!bearer_matches(&HeaderMap::new(), &tok));
    }

    /// Serves the real middleware + CORS stack on an ephemeral port and speaks raw HTTP to it.
    #[tokio::test]
    async fn gateway_stack_blocks_cross_site_and_unauthenticated_calls() {
        use axum::routing::{get, post};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "f".repeat(64);
        let auth = ApiAuth::new(token.clone(), port);
        let app = axum::Router::new()
            .route("/api/v1/health", get(|| async { "ok" }))
            .route("/api/v1/windows-mcp/call", post(|| async { "ran" }))
            .layer(axum::middleware::from_fn_with_state(auth.clone(), require_local_auth))
            .layer(auth.cors_layer());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let send = |raw: String| async move {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(raw.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf).to_string()
        };
        let req = |method: &str, path: &str, extra: &str| {
            format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
                 Content-Length: 0\r\n{extra}\r\n"
            )
        };
        let bearer = format!("Authorization: Bearer {token}\r\n");

        let r = send(req("GET", "/api/v1/health", "")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");

        let r = send(req("POST", "/api/v1/windows-mcp/call", "")).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");

        let r = send(req("POST", "/api/v1/windows-mcp/call", &bearer)).await;
        assert!(r.starts_with("HTTP/1.1 200") && r.ends_with("ran"), "{r}");

        let evil = format!("{bearer}Origin: https://evil.example\r\n");
        let r = send(req("POST", "/api/v1/windows-mcp/call", &evil)).await;
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");

        let rebind = format!(
            "POST /api/v1/windows-mcp/call HTTP/1.1\r\nHost: evil.example:{port}\r\n\
             Connection: close\r\nContent-Length: 0\r\n{bearer}\r\n"
        );
        let r = send(rebind).await;
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");

        let pre = "Origin: https://evil.example\r\nAccess-Control-Request-Method: POST\r\n";
        let r = send(req("OPTIONS", "/api/v1/windows-mcp/call", pre)).await;
        assert!(!r.to_ascii_lowercase().contains("access-control-allow-origin"), "{r}");

        let ui = "Origin: http://127.0.0.1:1420\r\nAccess-Control-Request-Method: POST\r\n";
        let r = send(req("OPTIONS", "/api/v1/windows-mcp/call", ui)).await;
        assert!(
            r.to_ascii_lowercase()
                .contains("access-control-allow-origin: http://127.0.0.1:1420"),
            "{r}"
        );
        // The browser may keep that answer for an hour instead of asking again before the next request.
        assert!(r.to_ascii_lowercase().contains("access-control-max-age: 3600"), "{r}");
    }

    /// Card OA1-04 (SE-19): the handler of the one public route can tell a caller that showed a token from one that did not (both have the lower scope).
    #[tokio::test]
    async fn the_public_route_can_tell_a_caller_that_showed_a_token_from_one_that_did_not() {
        use axum::routing::get;
        use tower::ServiceExt;
        let token = "e".repeat(64);
        let auth = ApiAuth::new(token.clone(), 8767);
        let app = axum::Router::new()
            .route("/api/v1/health", get(|axum::Extension(shown): axum::Extension<TokenShown>, axum::Extension(scope): axum::Extension<Scope>| async move { format!("{}:{:?}", shown.0, scope) }))
            .layer(axum::middleware::from_fn_with_state(auth, require_local_auth));
        let ask = |bearer: Option<String>| {
            let app = app.clone();
            async move {
                let mut req = axum::http::Request::builder().uri("/api/v1/health").header(header::HOST, "127.0.0.1:8767");
                if let Some(b) = bearer {
                    req = req.header(header::AUTHORIZATION, b);
                }
                let resp = app.oneshot(req.body(axum::body::Body::empty()).unwrap()).await.unwrap();
                let status = resp.status();
                let bytes = http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
                (status, String::from_utf8_lossy(&bytes).to_string())
            }
        };
        assert_eq!(ask(None).await, (StatusCode::OK, "false:Api".to_string()), "no token: the route answers, and says so to its handler");
        assert_eq!(ask(Some("Bearer wrong".into())).await.1, "false:Api", "a wrong token is no token");
        assert_eq!(ask(Some(format!("Bearer {token}"))).await, (StatusCode::OK, "true:Api".to_string()));
    }

    /// Card OA1-04 (SE-18): the UI scope wins over the API scope, so a UI token equal to the API token would give every API caller the right to approve and change policy.
    #[test]
    fn equal_tokens_are_refused_and_a_pair_that_differs_is_not() {
        let same = "c".repeat(64);
        assert!(check_distinct(&same, &same).is_err());
        assert!(check_distinct(&same, &"d".repeat(64)).is_ok());
        // Through the files: the UI token file holds the API token.
        let dir = std::env::temp_dir().join(format!("aqua_equal_tokens_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(TOKEN_FILE), &same).unwrap();
        std::fs::write(dir.join(UI_TOKEN_FILE), &same).unwrap();
        let refused = load_or_create_ui_token(&dir);
        assert!(refused.as_ref().is_err_and(|e| e.to_string().contains("the same as the API token")), "{refused:?}");
        assert!(!refused.unwrap_err().to_string().contains(&same), "the error does not print the token");
        let _ = std::fs::remove_dir_all(&dir);
        // And below the loaders: a pair that reached `ApiAuth` equal gives the UI scope to nothing.
        let auth = ApiAuth::new(same.clone(), 8767).with_ui_token(same.clone());
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {same}")).unwrap());
        assert_eq!(auth.scope_of(&h), Some(Scope::Api), "the API token stays the API token");
    }

    /// Card OA1-04 (SE-18): a token file that is made here has one account in its access list from the start, and a malformed file in its place is replaced.
    #[cfg(windows)]
    #[test]
    fn a_new_token_file_is_private_from_the_start_and_a_bad_one_is_replaced() {
        let dir = std::env::temp_dir().join(format!("aqua_private_token_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join(TOKEN_FILE);
        std::fs::write(&bad, "not a token").unwrap();
        std::env::remove_var(TOKEN_ENV);
        let token = load_or_create_token(&dir).unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(std::fs::read_to_string(&bad).unwrap().trim(), token, "the bad file was replaced");
        let out = std::process::Command::new(aqua_native_tools::sandbox::icacls_exe()).arg(&bad).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).replace(&*bad.to_string_lossy(), "");
        let entries: Vec<&str> = text.lines().filter(|l| l.contains(":(") && !l.contains("Mandatory Label")).collect();
        assert_eq!(entries.len(), 1, "one account in the list: {text}");
        for group in ["Everyone", "Users", "Authenticated Users"] {
            assert!(!text.contains(group), "{group} can open the token file: {text}");
        }
        assert!(!entries[0].contains("(I)"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Card OA1-04 (SE-18, A6-51): the data folder can be limited to this account, with what is in it.
    #[cfg(windows)]
    #[test]
    fn a_hardened_folder_lists_one_account_and_what_is_in_it_inherits_that() {
        let dir = std::env::temp_dir().join(format!("aqua_harden_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inside.txt"), "x").unwrap();
        harden_data_dir(&dir).unwrap();
        for target in [dir.clone(), dir.join("inside.txt")] {
            let out = std::process::Command::new(aqua_native_tools::sandbox::icacls_exe()).arg(&target).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).replace(&*target.to_string_lossy(), "");
            let entries: Vec<&str> = text.lines().filter(|l| l.contains(":(") && !l.contains("Mandatory Label")).collect();
            assert_eq!(entries.len(), 1, "{}: one account in the list: {text}", target.display());
            for group in ["Everyone", "Users", "Authenticated Users"] {
                assert!(!text.contains(group), "{group} can open {}: {text}", target.display());
            }
        }
        assert_eq!(std::fs::read_to_string(dir.join("inside.txt")).unwrap(), "x", "this account still reads it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_file_is_created_and_reused() {
        let dir = std::env::temp_dir().join(format!(
            "aqua_auth_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let first = load_or_create_token(&dir).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(load_or_create_token(&dir).unwrap(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
