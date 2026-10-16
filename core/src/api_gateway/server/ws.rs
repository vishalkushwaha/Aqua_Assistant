//! The WebSocket route (`/ws`, card OA3-06, moved out of `server.rs`): it counts the audio frames it is sent and does not run them through the pipeline (that is `/pipeline/wav`).

use super::*;

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    // A message is read whole before the handler sees it, so the limit has to be set here, not checked afterwards
    // (the library default is 64 MiB per message). Just above the largest audio frame the handler accepts.
    ws.max_message_size(2 * 1024 * 1024).max_frame_size(2 * 1024 * 1024).on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    info!("Websocket client connected to Aqua API Gateway");
    let welcome = serde_json::json!({
        "event": "CONNECTED",
        "payload": { "message": "Connected to Aqua Core Gateway (Binary PCM Audio Enabled)" }
    });

    if socket.send(Message::Text(welcome.to_string().into())).await.is_err() {
        return;
    }

    // Calculated maximum frame size: 10s @ 48kHz mono PCM16 = 960,000 bytes (allowing up to 20s / stereo max bound: 1,920,000 bytes)
    const MAX_WS_FRAME_SIZE: usize = 1_920_000;

    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Binary(bin_bytes) => {
                let frame_len = bin_bytes.len();
                if frame_len > MAX_WS_FRAME_SIZE {
                    let err_event = serde_json::json!({
                        "event": "ERROR",
                        "payload": { "message": "Frame exceeds 10s audio duration limit (1.92 MB)", "status": "REJECTED" }
                    });
                    let _ = socket.send(Message::Text(err_event.to_string().into())).await;
                    continue;
                }
                if frame_len % 2 != 0 {
                    let err_event = serde_json::json!({
                        "event": "ERROR",
                        "payload": { "message": "Odd byte count for PCM16 audio frame", "status": "REJECTED" }
                    });
                    let _ = socket.send(Message::Text(err_event.to_string().into())).await;
                    continue;
                }

                let event = serde_json::json!({
                    "event": "AUDIO_FRAME_RECEIVED",
                    // The socket counts frames; it does not run them through the pipeline (that is `/pipeline/wav`).
                    "payload": { "bytes": frame_len, "status": "RECEIVED", "processed": false }
                });
                if socket.send(Message::Text(event.to_string().into())).await.is_err() {
                    break;
                }
            }
            Message::Text(txt) => {
                if txt.contains("PING") {
                    let pong = serde_json::json!({ "event": "PONG" });
                    if socket.send(Message::Text(pong.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ws", get(ws_handler))
}
