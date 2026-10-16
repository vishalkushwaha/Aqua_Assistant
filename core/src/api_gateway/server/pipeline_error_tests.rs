use super::describe_pipeline_error as d;
use axum::http::StatusCode;

async fn body_of(response: axum::response::Response) -> (StatusCode, serde_json::Value) {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Card OA10-02, task UX-04: every error body has the sentence it always had and a `code` that does not change.
#[tokio::test]
async fn error_bodies_carry_a_code() {
    use super::{api_error, api_error_code, pipeline_failure};
    use crate::api_gateway::contract::ErrorCode;
    // A handler that says only the status gets the code of the status; the sentence is untouched.
    for (status, code) in [
        (StatusCode::BAD_REQUEST, "invalid_request"),
        (StatusCode::FORBIDDEN, "forbidden_scope"),
        (StatusCode::NOT_FOUND, "not_found"),
        (StatusCode::CONFLICT, "conflict"),
        (StatusCode::GONE, "too_late"),
        (StatusCode::PAYLOAD_TOO_LARGE, "invalid_request"),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request"),
        (StatusCode::TOO_MANY_REQUESTS, "limit_reached"),
        (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        (StatusCode::BAD_GATEWAY, "internal"),
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready"),
        (StatusCode::GATEWAY_TIMEOUT, "timeout"),
    ] {
        let (got, body) = body_of(api_error(status, "a sentence a person can read")).await;
        assert_eq!((got, body["error"].as_str(), body["code"].as_str()), (status, Some("a sentence a person can read"), Some(code)), "{status}: {body}");
    }
    // A handler that knows better says so, with a hint that is a fixed sentence.
    let (status, body) = body_of(api_error_code(StatusCode::CONFLICT, ErrorCode::Cancelled, "The task was stopped.", None)).await;
    assert_eq!((status, body), (StatusCode::CONFLICT, serde_json::json!({ "error": "The task was stopped.", "code": "cancelled" })));
    // The failures of a turn: the status they always had, and the code of what went wrong.
    for (raw, status, code, retryable) in [
        ("whisper-cli timed out after 90s", StatusCode::GATEWAY_TIMEOUT, "timeout", true),
        ("stopped: the task was cancelled", StatusCode::CONFLICT, "cancelled", false),
        ("speech-to-text is not available: the Whisper model is missing (see data/models/whisper/README.md)", StatusCode::SERVICE_UNAVAILABLE, "model_unavailable", false),
        ("the tool host is not available", StatusCode::SERVICE_UNAVAILABLE, "not_ready", true),
        ("empty wav_base64", StatusCode::BAD_REQUEST, "invalid_request", false),
        ("Kokoro CLI at C:\\py\\kokoro-tts.exe: kokoro-tts failed: Traceback", StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
    ] {
        let (got, body) = body_of(pipeline_failure(raw)).await;
        assert_eq!((got, body["code"].as_str(), body["retryable"].as_bool().unwrap_or(false)), (status, Some(code), retryable), "{raw}: {body}");
        assert!(body["error"].as_str().is_some_and(|e| !e.is_empty() && !e.contains("C:\\")), "{body}");
        // The status and the sentence are the ones `describe_pipeline_error` always gave.
        let (old_status, old_message) = d(raw);
        assert_eq!((old_status, body["error"].as_str()), (status, Some(old_message.as_str())), "{raw}");
    }
    // The hint of a code is a fixed sentence and carries nothing of the failure.
    let (_, body) = body_of(pipeline_failure("whisper-cli timed out after 90s: C:\\Users\\x\\a.wav")).await;
    assert!(body["hint"].as_str().is_some_and(|h| !h.contains("Users")), "{body}");
    assert!(!body["error"].as_str().unwrap().contains("Users"), "{body}");
}

#[test]
fn errors_get_a_real_status_and_never_show_folder_paths() {
    let (st, msg) = d("speech-to-text is not available: the Whisper model is missing (see data/models/whisper/README.md)");
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert!(msg.contains("speech-to-text is not available"));
    let (st, msg) = d("whisper transcribe \"C:\\Users\\lenovo\\AppData\\Local\\Temp\\aqua_pipeline_1\\df\\x.wav\"");
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!msg.contains("lenovo") && !msg.contains("AppData") && msg.contains("<path>"), "{msg}");
    assert_eq!(d("whisper-cli timed out after 90s").0, StatusCode::GATEWAY_TIMEOUT);
    // The whole chain is classified, and the folder in the outer context is still hidden.
    let (st, msg) = d(r#"whisper transcribe "C:\Users\x\a.wav": whisper-cli timed out after 90s"#);
    assert_eq!(st, StatusCode::GATEWAY_TIMEOUT, "{msg}");
    assert!(msg.contains("timed out") && !msg.contains("Users"), "{msg}");
    let (st, _) = d(r"Kokoro CLI at C:\py\kokoro-tts.exe: kokoro-tts failed: Traceback (most recent call last):");
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(d("empty wav_base64").0, StatusCode::BAD_REQUEST);
    let (_, long) = d(&"x".repeat(2000));
    assert_eq!(long.len(), 300);
    assert_eq!(d("").1, "the request failed");
    // Only the first line is shown: stderr dumps stay in the log.
    assert!(!d("whisper-cli failed: bad\nstack frame 1\nstack frame 2").1.contains("stack frame"));
}

#[test]
fn silence_and_a_muted_microphone_are_not_enough_to_enrol_a_voice() {
    assert_eq!(super::voiced_seconds(&vec![0.0; 16_000 * 5]), 0.0);
    assert_eq!(super::voiced_seconds(&vec![0.001; 16_000 * 5]), 0.0, "room noise");
    let speech: Vec<f32> = (0..16_000 * 3).map(|i| (i as f32 * 0.05).sin() * 0.2).collect();
    assert!((super::voiced_seconds(&speech) - 3.0).abs() < 0.05);
    let mut mixed = vec![0.0; 16_000 * 4];
    mixed.extend(&speech);
    assert!((super::voiced_seconds(&mixed) - 3.0).abs() < 0.05, "only the loud part counts");
}

#[test]
fn a_failed_request_can_be_retried_at_once_and_a_repeat_does_not_extend_the_window() {
    let k = super::audio_key(&("retry-".to_string() + &"x".repeat(40)));
    assert!(!super::is_duplicate_key(k));
    assert!(super::is_duplicate_key(k), "the same audio while the first is fresh");
    super::forget_audio(k);
    assert!(!super::is_duplicate_key(k), "after a failure (busy, error) the retry is a new request");
}

#[test]
fn the_same_audio_twice_in_a_row_is_recognised_but_different_audio_is_not() {
    let a = "QUJD".repeat(50) + "unique-a";
    assert!(!super::is_duplicate_audio(&a));
    assert!(super::is_duplicate_audio(&a), "the second copy is a duplicate");
    assert!(!super::is_duplicate_audio(&("different".to_string() + &a)));
}
