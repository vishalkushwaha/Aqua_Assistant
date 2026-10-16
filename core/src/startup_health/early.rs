//! Bind first (card R3-07). Aqua used to bind its port only after everything slow had run: the data folder, the engines, the checks (the speech daemon and its test
//! sentence, the browser, the Windows tool server when it is switched on). Audit 5 measured 12.9 seconds in which a client that asked "are you there?" was refused as if
//! nothing was running, and a second Aqua started in that time passed the check that the port is free (Audit 4, B4).
//!
//! Now the port is taken at the start and answered at once, by a small server on a thread of its own, until the real gateway is ready and takes the same socket over:
//!
//! * `GET /api/v1/health` and `GET /api/v1/status` answer **503** with `Retry-After` and a body that says `"status": "STARTING"` and `"startup_state": "STARTING"`, with
//!   every `*_ready` field of the status false. 503 and not 200 on purpose: every client that waits for a 200 (the start scripts, the harness, the app's `res.ok`) goes on
//!   waiting, as it did when the connection was refused, and none of them takes "starting" for "ready". The body is what lets a client tell "starting" from "not there".
//! * Every other route answers the same 503 (to a caller that has a token).
//! * The same host, origin and token rules as the real gateway: the stub runs the very `require_local_auth` the gateway runs, so it is no way around them. The health
//!   route needs no token, as before; the others need one, as before.
//! * The socket is shared, not rebound: the thread serves on a duplicate of the listening socket and the original is kept, so a connection that comes in while the real
//!   gateway takes over waits in the queue of the socket and is accepted by it. Nothing is refused, and no second Aqua can take the port in between.
//!
//! It is OFF unless `AQUA_BIND_FIRST=on` (or `1`, `true`, `yes`): the port is then bound by the gateway when it is ready, as before. (It ships off until the hand-over of the socket has been seen on a second core; the card asks for it on.) Any failure of the early part (the tokens,
//! the thread, the socket) goes on the same way: the early listener is dropped and the gateway binds the port later.

use anyhow::{Context, Result};
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::api_gateway::auth::{require_local_auth, ApiAuth};

/// `AQUA_BIND_FIRST=on` takes the port first; without it Aqua starts as it did before this card.
pub const BIND_FIRST_ENV: &str = "AQUA_BIND_FIRST";

/// How long a client that is in the middle of a request when the real gateway takes over is given to be answered. The answers are one small reply, so this is long.
const HANDOVER_GRACE: Duration = Duration::from_millis(1500);

/// Whether the port is taken first: only when `AQUA_BIND_FIRST` says `on`, `1`, `true` or `yes`.
pub fn enabled() -> bool {
    enabled_from(std::env::var(BIND_FIRST_ENV).ok().as_deref())
}

pub fn enabled_from(value: Option<&str>) -> bool {
    matches!(value.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true" | "yes"))
}

/// Takes the port. A port that is held already is `AddrInUse`, as the bind of the gateway said.
pub fn claim_port(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
}

/// What every answer says while Aqua starts: the field names of the status route, none of them ready.
pub fn starting_body(since: Duration) -> serde_json::Value {
    serde_json::json!({
        "status": "STARTING",
        "startup_state": "STARTING",
        "version": crate::version(),
        "database_ready": false,
        "noise_engine_ready": false,
        "whisper_engine_ready": false,
        "llm_engine_ready": false,
        "tts_engine_ready": false,
        "pipeline_ready": false,
        "text_pipeline_ready": false,
        "starting_for_ms": since.as_millis() as u64,
        "note": "Aqua is starting. Try again in a moment.",
    })
}

fn starting_response(since: Duration) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "1")], Json(starting_body(since))).into_response()
}

/// The routes of the first seconds: everything answers "starting", behind the gateway's own checks of host, origin and token.
pub fn starting_router(auth: ApiAuth, started: Instant) -> Router {
    let cors = auth.cors_layer();
    let answer = move || async move { starting_response(started.elapsed()) };
    Router::new()
        .route("/api/v1/health", get(answer))
        .route("/api/v1/status", get(answer))
        .fallback(answer)
        .layer(axum::middleware::from_fn_with_state(auth, require_local_auth))
        .layer(cors)
}

async fn stop_requested(mut stop: watch::Receiver<bool>) {
    while !*stop.borrow_and_update() {
        if stop.changed().await.is_err() {
            return; // the sender is gone: that is a stop too
        }
    }
}

async fn serve_starting(socket: TcpListener, auth: ApiAuth, started: Instant, stop: watch::Receiver<bool>) {
    let listener = match tokio::net::TcpListener::from_std(socket) {
        Ok(listener) => listener,
        Err(e) => {
            warn!("the early gateway could not use the port: {e}");
            return;
        }
    };
    let app = starting_router(auth, started);
    let for_grace = stop.clone();
    let server = async move {
        if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(stop_requested(stop)).await {
            warn!("the early gateway stopped with an error: {e}");
        }
    };
    tokio::pin!(server);
    let grace = async move {
        stop_requested(for_grace).await;
        tokio::time::sleep(HANDOVER_GRACE).await;
    };
    tokio::select! {
        _ = &mut server => {}
        _ = grace => warn!("the early gateway was still answering a client after {} ms; it is closed", HANDOVER_GRACE.as_millis()),
    }
}

/// The early listener: answers "starting" on the port until `hand_over`.
pub struct EarlyGateway {
    socket: Option<TcpListener>,
    stop: Option<watch::Sender<bool>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EarlyGateway {
    /// Starts answering on `socket` (a port that was claimed) with the rules of `auth`. If it cannot, the socket is dropped and the port is free again.
    pub fn start(socket: TcpListener, auth: ApiAuth) -> Result<Self> {
        let for_thread = socket.try_clone().context("share the listening socket")?;
        // On Windows a duplicated socket starts in blocking mode whatever the original was (seen with a probe on 2026-10-08: the runtime's copy never answered),
        // so the mode is set on the handle that the runtime will use. The original is made non-blocking when it is handed over.
        for_thread.set_nonblocking(true).context("make the listening socket non-blocking")?;
        let (stop, stopped) = watch::channel(false);
        let started = Instant::now();
        let thread = std::thread::Builder::new()
            .name("aqua-early-gateway".into())
            .spawn(move || {
                // A runtime of its own: the start-up work on the main thread blocks, and must not be able to starve this.
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        warn!("the early gateway could not start its runtime: {e}");
                        return;
                    }
                };
                runtime.block_on(serve_starting(for_thread, auth, started, stopped));
            })
            .context("start the early gateway thread")?;
        Ok(Self { socket: Some(socket), stop: Some(stop), thread: Some(thread) })
    }

    /// Stops answering "starting" and gives the same socket, still bound, to the real gateway. A client that is connecting meanwhile waits in the queue of the socket.
    /// Call it inside the tokio runtime that will serve.
    pub fn hand_over(mut self) -> Result<tokio::net::TcpListener> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(true);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join(); // at most the grace period: the thread closes whatever is still open then
        }
        let socket = self.socket.take().context("the early gateway has no socket to hand over")?;
        socket.set_nonblocking(true).context("make the listening socket non-blocking")?;
        tokio::net::TcpListener::from_std(socket).context("give the port to the gateway")
    }
}

impl Drop for EarlyGateway {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(true);
        }
    }
}

/// The start of a start: take the port, make the authentication (`make_auth` gets the port that was taken: the tokens are read or created there), and begin to answer.
/// `Err` only when something else holds the port already (the caller exits, as it did when its check found another Aqua). `Ok(None)` when the early part could not be done:
/// the port is released and everything goes on as it did before this card, the gateway binding the port when it is ready.
pub fn begin(port: u16, make_auth: impl FnOnce(u16) -> Result<ApiAuth>) -> io::Result<Option<EarlyGateway>> {
    begin_with(|| claim_port(port), make_auth)
}

/// [`begin`] with the claim given, so that a test can claim a port of its own choosing and see in what order things happen.
pub fn begin_with(claim: impl FnOnce() -> io::Result<TcpListener>, make_auth: impl FnOnce(u16) -> Result<ApiAuth>) -> io::Result<Option<EarlyGateway>> {
    let socket = match claim() {
        Ok(socket) => socket,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Err(e),
        Err(e) => {
            warn!("the port could not be taken early ({e}); the gateway will take it when it is ready, as before");
            return Ok(None);
        }
    };
    let bound = match socket.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            warn!("the port that was taken could not be read ({e}); the gateway will take it when it is ready, as before");
            return Ok(None);
        }
    };
    let auth = match make_auth(bound) {
        Ok(auth) => auth,
        Err(e) => {
            warn!("could not prepare the API tokens early: {e:#}");
            return Ok(None);
        }
    };
    match EarlyGateway::start(socket, auth) {
        Ok(gateway) => {
            info!("answering \"starting\" on 127.0.0.1:{bound} until the gateway is ready");
            Ok(Some(gateway))
        }
        Err(e) => {
            warn!("the early gateway did not start ({e:#}); the gateway will take the port when it is ready, as before");
            Ok(None)
        }
    }
}
