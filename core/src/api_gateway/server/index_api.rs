//! The routes of the file index (card OA3-06, moved out of `server.rs`): its status and settings, pause, rebuild and forget.

use super::*;

/// The file index (folders, programs and shortcuts on this computer, for "open the Personal AI folder").
fn index_unavailable() -> Response {
    (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": "the file index is not running" }))).into_response()
}

async fn index_status_endpoint() -> Response {
    let Some(ix) = aqua_mcp_adapters::file_index::global() else { return index_unavailable() };
    Json(serde_json::json!({ "status": ix.status(), "settings": ix.settings() })).into_response()
}

/// Only the fields that are present change.
#[derive(Deserialize)]
pub struct IndexSettingsPatch {
    pub enabled: Option<bool>,
    pub index_documents: Option<bool>,
    pub index_files: Option<bool>,
    pub other_drives: Option<bool>,
    pub extra_roots: Option<Vec<String>>,
    pub exclude_paths: Option<Vec<String>>,
}

async fn index_settings_endpoint(Json(p): Json<IndexSettingsPatch>) -> Response {
    let Some(ix) = aqua_mcp_adapters::file_index::global() else { return index_unavailable() };
    let mut s = ix.settings();
    if let Some(v) = p.enabled {
        s.enabled = v;
    }
    if let Some(v) = p.index_documents {
        s.index_documents = v;
    }
    if let Some(v) = p.index_files {
        s.index_files = v;
    }
    if let Some(v) = p.other_drives {
        s.other_drives = v;
    }
    if let Some(v) = p.extra_roots {
        s.extra_roots = v;
    }
    if let Some(v) = p.exclude_paths {
        s.exclude_paths = v;
    }
    match ix.set_settings(s) {
        Ok(()) => Json(serde_json::json!({ "ok": true, "settings": ix.settings() })).into_response(),
        Err(e) => (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct IndexPauseRequest {
    pub paused: bool,
}

async fn index_pause_endpoint(Json(p): Json<IndexPauseRequest>) -> Response {
    let Some(ix) = aqua_mcp_adapters::file_index::global() else { return index_unavailable() };
    ix.set_paused(p.paused);
    Json(serde_json::json!({ "ok": true, "paused": p.paused })).into_response()
}

async fn index_rebuild_endpoint() -> Response {
    let Some(ix) = aqua_mcp_adapters::file_index::global() else { return index_unavailable() };
    ix.rebuild();
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn index_forget_endpoint() -> Response {
    let Some(ix) = aqua_mcp_adapters::file_index::global() else { return index_unavailable() };
    ix.forget();
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/index/status", get(index_status_endpoint))
        .route("/api/v1/index/settings", post(index_settings_endpoint))
        .route("/api/v1/index/pause", post(index_pause_endpoint))
        .route("/api/v1/index/rebuild", post(index_rebuild_endpoint))
        .route("/api/v1/index/forget", post(index_forget_endpoint))
}
