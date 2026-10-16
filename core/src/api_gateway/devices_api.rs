//! HTTP API for device and battery monitoring.
//!
//! Reading needs the API token. Anything that changes state (settings, a device's switches, forgetting, test and the player's
//! calls) needs the UI token, so a script holding only the API token cannot silence the monitor, ignore every device, or make
//! Aqua speak. Device references in the API are opaque (a salted hash): no Bluetooth address or serial number leaves the core.
//! Plan: `documentation/DEVICE_MONITOR_PLAN.md`; user guide: `documentation/DEVICES.md`.

use super::auth::{ui_scope_required, Scope};
use aqua_device_monitor::{global, Allow, Category, DevicePrefs, Monitor, RawEvent, Settings};
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/devices", get(get_devices))
        .route("/api/v1/devices/stream", get(stream))
        .route("/api/v1/devices/settings", post(post_settings))
        .route("/api/v1/devices/test", post(post_test))
        .route("/api/v1/devices/forget-all", post(post_forget_all))
        .route("/api/v1/devices/announcements/next", post(post_next))
        .route("/api/v1/devices/announcements/:id/ack", post(post_ack))
        .route("/api/v1/devices/simulate", post(post_simulate))
        .route("/api/v1/devices/:reference/settings", post(post_device_settings))
        .route("/api/v1/devices/:reference/forget", post(post_forget))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn monitor() -> Result<&'static std::sync::Arc<Monitor>, Response> {
    global().ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "device monitoring is not running"))
}

fn view(m: &Monitor) -> Value {
    let (engine_cpu, source_cpu) = m.cpu_seconds();
    let mut v = serde_json::to_value(m.snapshot()).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("cpu_seconds".into(), json!({ "engine": engine_cpu, "source": source_cpu }));
    }
    v
}

async fn get_devices() -> Response {
    match monitor() {
        Ok(m) => Json(view(m)).into_response(),
        Err(e) => e,
    }
}

#[derive(Deserialize)]
struct StreamQuery {
    after: Option<u64>,
}

/// A stream of full snapshots: the first one at once, then one whenever something changes. Each carries an id (the version), so
/// a client that reconnects with `?after=<id>` and finds nothing new is not sent the same snapshot again; a client that missed
/// changes simply gets the current state (what matters is the state, and the announcement queue is owned by the core).
async fn stream(Query(q): Query<StreamQuery>) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let m = match monitor() {
        Ok(m) => m,
        Err(e) => return e,
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(8);
    let mut sent = q.after.unwrap_or(0);
    tokio::spawn(async move {
        loop {
            // The only exit used to be a failed send, which was tried only when something had changed: a tab that was closed left this task
            // asking the monitor twice a second until the next device event.
            if tx.is_closed() {
                return;
            }
            let version = tokio::task::spawn_blocking(move || m.version()).await.unwrap_or(0);
            if version != sent {
                let payload = tokio::task::spawn_blocking(move || view(m)).await.unwrap_or(Value::Null);
                let ev = Event::default().event("snapshot").id(version.to_string()).data(payload.to_string());
                if tx.send(Ok(ev)).await.is_err() {
                    return; // the client went away
                }
                sent = version;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

/// Change general settings. Only the fields present change; the per-device switches are untouched.
async fn post_settings(axum::Extension(scope): axum::Extension<Scope>, Json(patch): Json<Value>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let m = match monitor() {
        Ok(m) => m,
        Err(e) => return e,
    };
    let Some(patch) = patch.as_object() else { return error(StatusCode::BAD_REQUEST, "send a JSON object") };
    let mut current = serde_json::to_value(m.snapshot().settings).unwrap_or(Value::Null);
    let Some(obj) = current.as_object_mut() else { return error(StatusCode::INTERNAL_SERVER_ERROR, "settings are unreadable") };
    for (k, v) in patch {
        if k == "devices" {
            continue; // per-device switches are changed one device at a time
        }
        if !obj.contains_key(k) {
            return error(StatusCode::BAD_REQUEST, format!("unknown setting '{k}'"));
        }
        obj.insert(k.clone(), v.clone());
    }
    let settings: Settings = match serde_json::from_value(current) {
        Ok(s) => s,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("invalid settings: {e}")),
    };
    // The per-device map is kept as it is.
    let mut settings = settings;
    settings.devices = m.snapshot().settings.devices;
    let _unused: Option<Category> = None;
    match m.set_settings(settings) {
        Ok(()) => Json(view(m)).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn post_device_settings(axum::Extension(scope): axum::Extension<Scope>, Path(reference): Path<String>, Json(prefs): Json<DevicePrefs>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let m = match monitor() {
        Ok(m) => m,
        Err(e) => return e,
    };
    match m.set_device_prefs(&reference, prefs) {
        Ok(()) => Json(view(m)).into_response(),
        Err(e) => error(StatusCode::NOT_FOUND, e),
    }
}

async fn post_forget(axum::Extension(scope): axum::Extension<Scope>, Path(reference): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let m = match monitor() {
        Ok(m) => m,
        Err(e) => return e,
    };
    if m.forget(&reference) {
        Json(view(m)).into_response()
    } else {
        error(StatusCode::NOT_FOUND, "unknown device")
    }
}

async fn post_forget_all(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let m = match monitor() {
        Ok(m) => m,
        Err(e) => return e,
    };
    m.forget_all();
    Json(view(m)).into_response()
}

static LAST_TEST_MS: AtomicU64 = AtomicU64::new(0);

/// Queue a test announcement. Rate limited, so a script cannot make Aqua talk continuously.
async fn post_test(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let m = match monitor() {
        Ok(m) => m,
        Err(e) => return e,
    };
    let now = m.now_ms();
    let last = LAST_TEST_MS.load(Ordering::SeqCst);
    if last != 0 && now.saturating_sub(last) < 3_000 {
        return error(StatusCode::TOO_MANY_REQUESTS, "wait a few seconds between tests");
    }
    LAST_TEST_MS.store(now.max(1), Ordering::SeqCst);
    let id = m.test_announcement();
    Json(json!({ "queued": id })).into_response()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NextRequest {
    /// "all" or "critical_only" (the player is busy with an answer, or a task is thinking).
    allow: Option<String>,
}

/// The player asks for the next thing to say. 204 when there is nothing it may say now.
async fn post_next(axum::Extension(scope): axum::Extension<Scope>, body: Option<Json<NextRequest>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let m = match monitor() {
        Ok(m) => m,
        Err(e) => return e,
    };
    let allow = match body.and_then(|b| b.0.allow).as_deref() {
        Some("critical_only") => Allow::CriticalOnly,
        _ => Allow::All,
    };
    match tokio::task::spawn_blocking(move || m.next(allow)).await {
        Ok(Some(a)) => Json(json!({ "id": a.id, "text": a.text, "priority": a.priority, "kind": a.kind })).into_response(),
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

#[derive(Deserialize)]
struct AckRequest {
    spoken: bool,
}

async fn post_ack(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<u64>, Json(b): Json<AckRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let m = match monitor() {
        Ok(m) => m,
        Err(e) => return e,
    };
    m.ack(id, b.spoken);
    Json(json!({ "ok": true })).into_response()
}

// ---------------------------------------------------------------------------------------------------------------------
// The simulator: feeds scripted device events through the same engine, queue, stream and speaker as real ones, so the whole
// chain can be checked without unplugging anything. Only available when `AQUA_DEVICE_SIM=1`.

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Sim {
    Present {
        id: String,
        name: String,
        #[serde(default = "default_type")]
        device_type: aqua_device_monitor::DeviceType,
        battery: Option<u8>,
        #[serde(default)]
        charging: Option<aqua_device_monitor::Charging>,
    },
    Gone {
        id: String,
    },
    Battery {
        id: String,
        percent: u8,
        #[serde(default)]
        charging: Option<aqua_device_monitor::Charging>,
    },
}

fn default_type() -> aqua_device_monitor::DeviceType {
    aqua_device_monitor::DeviceType::Headphones
}

#[derive(Deserialize)]
struct SimRequest {
    events: Vec<Sim>,
}

async fn post_simulate(axum::Extension(scope): axum::Extension<Scope>, Json(req): Json<SimRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if std::env::var("AQUA_DEVICE_SIM").ok().as_deref() != Some("1") {
        return error(StatusCode::FORBIDDEN, "the simulator is off (start Aqua with AQUA_DEVICE_SIM=1)");
    }
    let m = match monitor() {
        Ok(m) => m,
        Err(e) => return e,
    };
    use aqua_device_monitor::{BatteryReading, BatterySource, Charging, Connection, DeviceInfo, IdKind};
    let mut n = 0;
    for e in req.events {
        let raw = match e {
            Sim::Present { id, name, device_type, battery, charging } => RawEvent::Present {
                info: DeviceInfo { id: format!("sim:{id}"), id_kind: IdKind::Weak, name, device_type, manufacturer: String::new(), connection: Connection::Bluetooth },
                battery: battery.map(|p| BatteryReading { percent: Some(p), charging: charging.unwrap_or(Charging::Unknown), source: BatterySource::WindowsProperty }),
            },
            Sim::Gone { id } => RawEvent::Gone { id: format!("sim:{id}") },
            Sim::Battery { id, percent, charging } => RawEvent::Battery {
                id: format!("sim:{id}"),
                reading: BatteryReading { percent: Some(percent), charging: charging.unwrap_or(Charging::Unknown), source: BatterySource::WindowsProperty },
            },
        };
        m.inject(raw);
        n += 1;
    }
    Json(json!({ "injected": n })).into_response()
}
