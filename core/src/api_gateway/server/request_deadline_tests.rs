use super::*;
use axum::{routing::get, Router};
use tower::ServiceExt;

fn app(limit: std::time::Duration) -> Router {
    Router::new()
        .route("/slow", get(|| async { tokio::time::sleep(std::time::Duration::from_millis(600)).await; "late" }))
        .route("/fast", get(|| async { "ok" }))
        .route("/api/v1/flows/runs/1/stream", get(|| async { tokio::time::sleep(std::time::Duration::from_millis(300)).await; "still open" }))
        .layer(axum::middleware::from_fn(move |req, next| deadline_for(req, next, limit)))
}

async fn status(app: Router, path: &str) -> (axum::http::StatusCode, String) {
    let resp = app.oneshot(axum::http::Request::builder().uri(path).body(axum::body::Body::empty()).unwrap()).await.unwrap();
    let code = resp.status();
    let bytes = http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
    (code, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn a_request_that_takes_too_long_is_stopped_with_a_504_and_a_quick_one_is_untouched() {
    let limit = std::time::Duration::from_millis(150);
    let (code, body) = status(app(limit), "/slow").await;
    assert_eq!(code, axum::http::StatusCode::GATEWAY_TIMEOUT);
    assert!(body.contains("took longer than"), "{body}");
    assert_eq!(status(app(limit), "/fast").await, (axum::http::StatusCode::OK, "ok".to_string()));
}

#[tokio::test]
async fn streams_are_never_cut_by_the_backstop() {
    let (code, body) = status(app(std::time::Duration::from_millis(100)), "/api/v1/flows/runs/1/stream").await;
    assert_eq!((code, body.as_str()), (axum::http::StatusCode::OK, "still open"));
    for p in ["/ws", "/api/v1/llama/generate_stream", "/api/v1/mcp/agent/stream", "/api/v1/flows/runs/3/stream"] {
        assert!(is_stream_path(p), "{p}");
    }
    for p in ["/api/v1/flows", "/api/v1/pipeline/wav", "/api/v1/status"] {
        assert!(!is_stream_path(p), "{p}");
    }
}
