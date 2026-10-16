//! The person's voice (card OA3-06, moved out of `server.rs`): the settings of the wake phrase and the speaker check, the enrolment of a voice, the check of one, and the voice session (`/voice/settings`, `/voice/enroll`, `/voice/verify`, `/voice/session`).

use super::*;

/// Speech needed in an enrolment recording, in seconds.
const MIN_VOICED_ENROLL_SECS: f32 = 2.0;

/// Seconds of audio that are clearly louder than a quiet room: 20 ms windows with an RMS above 0.01.
pub fn voiced_seconds(samples_16k: &[f32]) -> f32 {
    const WINDOW: usize = 320;
    let voiced = samples_16k
        .chunks(WINDOW)
        .filter(|w| w.len() == WINDOW && (w.iter().map(|s| s * s).sum::<f32>() / WINDOW as f32).sqrt() > 0.01)
        .count();
    voiced as f32 * WINDOW as f32 / 16_000.0
}

#[derive(Deserialize)]
pub struct VoiceSessionRequest {
    pub active: bool,
}

#[derive(Serialize)]
pub struct VoiceSessionResponse {
    pub session_active: bool,
}

/// Open or close the voice session on purpose. The desktop orb calls this when the person turns the conversation on
/// (a deliberate click, so the wake word is not needed), keeps it open while the conversation lasts, and closes it
/// when they leave. It sits behind the same local token as every other route; speaker verification still applies.
async fn voice_session_endpoint(Json(payload): Json<VoiceSessionRequest>) -> Json<VoiceSessionResponse> {
    let session = crate::voice_commands::voice_session();
    let now = unix_now();
    if payload.active {
        session.open(now);
    } else {
        session.close();
    }
    Json(VoiceSessionResponse { session_active: session.is_active(now) })
}

#[derive(Serialize, Deserialize)]
pub struct VoiceSettingsResponse {
    pub wakeup_word: String,
    pub custom_reply: String,
    pub biometric_enrolled: bool,
    pub enrolled_fingerprint_id: Option<String>,
    pub voice_security_enabled: bool,
}

#[derive(Deserialize)]
pub struct VoiceSettingsRequest {
    pub wakeup_word: String,
    pub custom_reply: String,
    pub voice_security_enabled: bool,
}

async fn voice_get_settings_endpoint(State(state): State<AppState>) -> Json<VoiceSettingsResponse> {
    let wakeup_word = state
        .db
        .get_setting("wakeup_word")
        .ok()
        .flatten()
        .unwrap_or_else(|| "Hey Aqua".to_string());

    let custom_reply = state
        .db
        .get_setting("custom_reply")
        .ok()
        .flatten()
        .unwrap_or_else(|| "Yes Master, I am online and listening.".to_string());

    let biometric_enrolled = state
        .db
        .get_setting("master_voice_enrolled")
        .ok()
        .flatten()
        .is_some_and(|v| v == "true" || v == "1");

    let enrolled_fingerprint_id = if biometric_enrolled {
        state
            .db
            .get_master_voice_embedding()
            .ok()
            .flatten()
            .map(|emb| voice_fingerprint_id(&emb))
    } else {
        None
    };

    let voice_security_enabled = state.db.voice_lock_enabled();

    Json(VoiceSettingsResponse {
        wakeup_word,
        custom_reply,
        biometric_enrolled,
        enrolled_fingerprint_id,
        voice_security_enabled,
    })
}

async fn voice_post_settings_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<VoiceSettingsRequest>,
) -> Response {
    // Switching speaker verification off changes who can command the assistant: UI token only.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    // A setting that did not save must not be reported as saved.
    let saved = state
        .db
        .set_setting("wakeup_word", &payload.wakeup_word)
        .and_then(|_| state.db.set_setting("custom_reply", &payload.custom_reply))
        .and_then(|_| state.db.set_setting("voice_security_enabled", if payload.voice_security_enabled { "true" } else { "false" }));
    if let Err(e) = saved {
        tracing::warn!("could not save the voice settings: {e:#}");
        return api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "the voice settings could not be saved");
    }

    Json(serde_json::json!({
        "status": "SUCCESS",
        "wakeup_word": payload.wakeup_word,
        "custom_reply": payload.custom_reply,
        "voice_security_enabled": payload.voice_security_enabled
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct VoiceEnrollRequest {
    pub wav_samples_base64: Vec<String>,
    pub wakeup_word: String,
    pub custom_reply: String,
}

#[derive(Serialize)]
pub struct VoiceEnrollResponse {
    pub success: bool,
    pub enrolled_fingerprint_id: String,
    pub message: String,
}

/// Short, stable display id for an enrolled voice embedding (FNV-1a over the f32 bits).
/// Not a security token — just lets the UI tell enrollments apart.
fn voice_fingerprint_id(embedding: &[f32]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in embedding {
        for b in v.to_bits().to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("ecapa-{h:016x}")
}

/// Auth is enforced for every route by `auth::require_local_auth`.
async fn voice_enroll_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<VoiceEnrollRequest>,
) -> Response {
    // Enrolling replaces the voice Aqua trusts. Only the UI (the person at the keyboard) may do that.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let state_clone = state.clone();
    let res = tokio::task::spawn_blocking(move || {
        let mut all_samples = Vec::new();

        let mut skipped = 0usize;
        for b64 in &payload.wav_samples_base64 {
            match decode_bytes_base64(b64).map_err(|e| e.to_string()).and_then(|bytes| extract_pcm16_samples(&bytes).map_err(|e| e.to_string())) {
                Ok(pcm) => all_samples.extend(pcm),
                Err(_) => skipped += 1,
            }
        }

        // Every clip is converted to 16 kHz mono first, so a 48 kHz or stereo recording makes the same voiceprint as a
        // 16 kHz one. A voiceprint from a second or two of audio is not a voiceprint of anybody.
        const MIN_ENROLL_SECS: usize = 3;
        const MAX_ENROLL_SECS: usize = 180;
        if all_samples.len() < MIN_ENROLL_SECS * 16_000 {
            return VoiceEnrollResponse {
                success: false,
                enrolled_fingerprint_id: String::new(),
                message: format!(
                    "Enrolment needs at least {MIN_ENROLL_SECS} seconds of audio; {:.1} seconds could be read{}.",
                    all_samples.len() as f32 / 16_000.0,
                    if skipped > 0 { format!(" ({skipped} clip(s) could not be decoded)") } else { String::new() }
                ),
            };
        }
        if all_samples.len() > MAX_ENROLL_SECS * 16_000 {
            return VoiceEnrollResponse {
                success: false,
                enrolled_fingerprint_id: String::new(),
                message: format!("Enrolment audio is longer than {MAX_ENROLL_SECS} seconds; send shorter clips."),
            };
        }
        if skipped > 0 {
            tracing::warn!("voice enrolment: {skipped} clip(s) could not be decoded and were left out");
        }
        // Silence, a muted microphone or noise is not a voice: enrolling it would replace the voice Aqua trusts with
        // nothing, and the person would be locked out after a "success" message.
        let voiced = voiced_seconds(&all_samples);
        if voiced < MIN_VOICED_ENROLL_SECS {
            return VoiceEnrollResponse {
                success: false,
                enrolled_fingerprint_id: String::new(),
                message: format!(
                    "I could hear only {voiced:.1} seconds of speech in that recording (I need at least {MIN_VOICED_ENROLL_SECS:.0}). Check that the microphone is on and not muted, speak normally, and try again. Nothing was changed."
                ),
            };
        }

        let mut bio = state_clone.biometrics.lock().unwrap_or_else(PoisonError::into_inner);
        // The recording is cut into parts and each is fingerprinted: parts that do not sound like one voice are refused
        // here, before anything is saved, so a bad recording cannot replace the voice Aqua already trusts.
        let print = match bio.enrolment_fingerprint(&all_samples) {
            Ok(p) => p,
            Err(e) => {
                return VoiceEnrollResponse {
                    success: false,
                    enrolled_fingerprint_id: String::new(),
                    message: format!("{e} Nothing was changed."),
                }
            }
        };
        let (embedding, enrol_parts, enrol_consistency) = (print.embedding, print.parts, print.consistency);

        // Save first. If it cannot be saved (the encryption key store is unavailable, the disk is full), enrolment
        // fails and the voice already trusted stays as it was, instead of "success" followed by a voice that is
        // gone after the next restart.
        if let Err(e) = state_clone.db.save_master_voice_profile(&embedding, &payload.wakeup_word, &payload.custom_reply) {
            tracing::warn!("voice enrolment could not be saved: {e:#}");
            return VoiceEnrollResponse {
                success: false,
                enrolled_fingerprint_id: String::new(),
                message: format!("The voice profile could not be saved, so nothing was changed: {e}"),
            };
        }
        if let Err(e) = bio.register_master_fingerprint(embedding.clone()) {
            return VoiceEnrollResponse {
                success: false,
                enrolled_fingerprint_id: String::new(),
                message: format!("Biometric registration failed: {e}"),
            };
        }

        VoiceEnrollResponse {
            success: true,
            enrolled_fingerprint_id: voice_fingerprint_id(&embedding),
            message: format!("Master voice biometric profile registered and persisted successfully ({enrol_parts} part(s) of your speech, {:.0}% consistent).", enrol_consistency * 100.0),
        }
    })
    .await
    .unwrap_or_else(|e| VoiceEnrollResponse {
        success: false,
        enrolled_fingerprint_id: String::new(),
        message: format!("Enrollment task error: {e}"),
    });

    Json(res).into_response()
}

#[derive(Deserialize)]
pub struct VoiceVerifyRequest {
    pub wav_base64: String,
}

#[derive(Serialize)]
pub struct VoiceVerifyResponse {
    pub authorized: bool,
    pub similarity_percentage: f32,
    pub speaker_label: String,
    pub action_taken: String,
}

async fn voice_verify_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<VoiceVerifyRequest>,
) -> Response {
    // The answer is a similarity score to the enrolled voice, to a tenth of a percent. Anyone who can ask for it freely
    // can use it to tune a recording until it passes, so only the UI (the person at the keyboard) may.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let state_clone = state.clone();
    let res = tokio::task::spawn_blocking(move || {
        let bytes = match decode_bytes_base64(&payload.wav_base64) {
            Ok(b) if !b.is_empty() => b,
            _ => {
                return VoiceVerifyResponse {
                    authorized: false,
                    similarity_percentage: 0.0,
                    speaker_label: "Invalid Audio Input".to_string(),
                    action_taken: "REJECTED: Base64 decode failed".to_string(),
                }
            }
        };

        let pcm_samples = match extract_pcm16_samples(&bytes) {
            Ok(pcm) if !pcm.is_empty() => pcm,
            _ => {
                return VoiceVerifyResponse {
                    authorized: false,
                    similarity_percentage: 0.0,
                    speaker_label: "Silent / Empty Audio".to_string(),
                    action_taken: "REJECTED: Empty PCM stream".to_string(),
                }
            }
        };

        let mut bio = state_clone.biometrics.lock().unwrap_or_else(PoisonError::into_inner);

        if !bio.is_master_registered() {
            if let Ok(Some(saved_emb)) = state_clone.db.get_master_voice_embedding() {
                let _ = bio.register_master_fingerprint(saved_emb);
            }
        }

        let speaker_embedding = match bio.extract_embedding(&pcm_samples) {
            Ok(emb) => emb,
            Err(e) => {
                return VoiceVerifyResponse {
                    authorized: false,
                    similarity_percentage: 0.0,
                    speaker_label: "Biometric Extraction Failure".to_string(),
                    action_taken: format!("REJECTED: {e}"),
                }
            }
        };

        let (authorized, similarity) = bio.verify_speaker(&speaker_embedding);
        let sim_pct = (similarity * 100.0).clamp(0.0, 100.0);
        let custom_reply = state_clone
            .db
            .get_setting("custom_reply")
            .ok()
            .flatten()
            .unwrap_or_else(|| "Yes Master, I am online and listening.".to_string());

        let security_enabled = state_clone.db.voice_lock_enabled();

        if !security_enabled {
            VoiceVerifyResponse {
                authorized: true,
                similarity_percentage: (sim_pct * 10.0).round() / 10.0,
                speaker_label: "Anyone (Security Disabled)".to_string(),
                action_taken: format!("Voice Security Disabled. Triggering response: \"{custom_reply}\""),
            }
        } else if authorized {
            VoiceVerifyResponse {
                authorized: true,
                similarity_percentage: (sim_pct * 10.0).round() / 10.0,
                speaker_label: "Master Admin (Authorized Speaker)".to_string(),
                action_taken: format!("Voice Matched! Triggering response: \"{custom_reply}\""),
            }
        } else {
            VoiceVerifyResponse {
                authorized: false,
                similarity_percentage: (sim_pct * 10.0).round() / 10.0,
                speaker_label: "Unknown / Unauthorized Speaker".to_string(),
                action_taken: format!(
                    "ACCESS DENIED: Similarity ({:.1}%) below {:.0}% threshold. Command rejected.",
                    sim_pct,
                    bio.match_threshold() * 100.0
                ),
            }
        }
    })
    .await
    .unwrap_or_else(|e| VoiceVerifyResponse {
        authorized: false,
        similarity_percentage: 0.0,
        speaker_label: "System Task Execution Error".to_string(),
        action_taken: format!("REJECTED: {e}"),
    });

    Json(res).into_response()
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/voice/session", post(voice_session_endpoint))
        .route("/api/v1/voice/settings", get(voice_get_settings_endpoint).post(voice_post_settings_endpoint))
        .route("/api/v1/voice/enroll", post(voice_enroll_endpoint).layer(axum::extract::DefaultBodyLimit::max(AUDIO_BODY_LIMIT)))
        .route("/api/v1/voice/verify", post(voice_verify_endpoint).layer(axum::extract::DefaultBodyLimit::max(AUDIO_BODY_LIMIT)))
}
