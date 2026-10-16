//! `GET` and `PUT /api/v1/voice/choice`: which of the speech engine's voices Aqua speaks with, kept across restarts (card R4-03).
//!
//! Until now the voice was a default written in the code (`af_sarah`) that a request could override and nothing remembered. The choice is kept in `data/voice_choice.json`
//! (`{"voice": "af_nicole"}`) and is what the three request types that carry a `voice` (the text and voice pipelines, the speech route) use when the request does not name one.
//! A request that names a voice still gets that voice. Without the file, or with a file that is not a voice, the default is what it always was.
//!
//! * `GET` (either token) gives the choice, the default, and the voices the speech engine has (read from the names inside its voices file), or why they could not be read.
//! * `PUT` (the UI token) takes `{"voice": "af_nicole"}`, checks that the name is one of the engine's voices, and keeps it. `{"voice": null}` goes back to the default. A voice
//!   that is not in the engine's file is refused (a wrong name would make every answer fail to be spoken), and so is any change while the file of voices cannot be read.
//! * Only English voices (`a*` American, `b*` British, the first letter of the name) for now: the language the speech engine is told follows the voice, and the others need a
//!   language setting that does not exist yet (decision D-10). The language is not chosen separately.
//!
//! The choice is read from the file once and kept in memory; the route writes both.

use super::auth::{ui_scope_required, Scope};
use axum::{
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const DEFAULT_VOICE: &str = "af_sarah";
pub const DEFAULT_LANG: &str = "en-us";
const FILE: &str = "voice_choice.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub voice: String,
    pub lang: String,
}

impl Choice {
    fn default_choice() -> Self {
        Self { voice: DEFAULT_VOICE.to_string(), lang: DEFAULT_LANG.to_string() }
    }
}

/// The language the speech engine is told for an English voice: `en-us` for an American one (`a*`), `en-gb` for a British one (`b*`). `None` for any other name.
pub fn lang_of(voice: &str) -> Option<&'static str> {
    if !valid_voice_name(voice) {
        return None;
    }
    match voice.as_bytes()[0] {
        b'a' => Some("en-us"),
        b'b' => Some("en-gb"),
        _ => None,
    }
}

/// `af_nicole`, `bm_george`: an English voice's name (accent, then `f` or `m`, an underscore, a name of 2 to 20 lower-case letters and digits).
pub fn valid_voice_name(voice: &str) -> bool {
    let b = voice.as_bytes();
    b.len() >= 5 && b.len() <= 23 && matches!(b[0], b'a' | b'b') && matches!(b[1], b'f' | b'm') && b[2] == b'_' && b[3..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The choice kept in `dir`, or the default when there is none (or what is kept is not an English voice's name).
pub fn load_from(dir: &Path) -> Choice {
    let kept = std::fs::read_to_string(dir.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok())
        .and_then(|v| v.get("voice").and_then(|s| s.as_str()).map(str::to_string));
    match kept.and_then(|voice| lang_of(&voice).map(|lang| Choice { voice, lang: lang.to_string() })) {
        Some(choice) => choice,
        None => Choice::default_choice(),
    }
}

/// Keeps `voice` in `dir` (written whole under another name, then renamed), or goes back to the default when it is `None`.
pub fn save_to(dir: &Path, voice: Option<&str>) -> Result<Choice, String> {
    let path = dir.join(FILE);
    let Some(voice) = voice else {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("the voice choice could not be forgotten: {e}")),
        }
        return Ok(Choice::default_choice());
    };
    let lang = lang_of(voice).ok_or_else(|| format!("{voice:?} is not the name of an English voice (an accent letter a or b, f or m, an underscore and a name: af_nicole)"))?;
    aqua_tool_host::fsutil::write_atomic_durable(&path, &serde_json::to_vec_pretty(&json!({ "voice": voice })).unwrap_or_default()).map_err(|e| format!("the voice choice could not be kept: {e}"))?;
    Ok(Choice { voice: voice.to_string(), lang: lang.to_string() })
}

/// The voices the speech engine has: the names inside its voices file (a zip of one array a voice, `af_sarah.npy` ...), sorted.
pub fn voices_in(models_dir: &Path) -> Result<Vec<String>, String> {
    let file = aqua_audio_adapter::kokoro_cli::resolve_voices_file(models_dir).ok_or_else(|| format!("the speech engine's voices file is not in {}", models_dir.display()))?;
    let archive = zip::ZipArchive::new(std::fs::File::open(&file).map_err(|e| format!("{} could not be opened: {e}", file.display()))?).map_err(|e| format!("{} is not a file of voices: {e}", file.display()))?;
    let mut names: Vec<String> = archive.file_names().take(2_000).filter_map(|n| n.strip_suffix(".npy")).map(str::to_string).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Err(format!("{} holds no voices", file.display()));
    }
    Ok(names)
}

static CURRENT: Mutex<Option<Choice>> = Mutex::new(None);

/// The voice and language the requests use when they name none: the choice kept in the data folder, read once; the default when there is none.
pub fn current() -> Choice {
    let mut slot = CURRENT.lock().unwrap_or_else(|p| p.into_inner());
    slot.get_or_insert_with(|| load_from(&crate::runtime::data_dir())).clone()
}

/// Where the choice is kept and where the voices are (a test gives its own folders).
#[derive(Clone, Default)]
pub struct Dirs {
    data: Option<PathBuf>,
    models: Option<PathBuf>,
}

impl Dirs {
    pub fn at(data: PathBuf, models: PathBuf) -> Self {
        Self { data: Some(data), models: Some(models) }
    }

    fn data(&self) -> PathBuf {
        self.data.clone().unwrap_or_else(crate::runtime::data_dir)
    }

    fn models(&self) -> PathBuf {
        self.models.clone().unwrap_or_else(|| crate::runtime::default_models_dir().join("kokoro"))
    }
}

/// The voice that is used, the default, and the voices there are.
async fn get_choice(axum::Extension(dirs): axum::Extension<Dirs>) -> Response {
    let choice = if dirs.data.is_none() { current() } else { load_from(&dirs.data()) };
    let models = dirs.models();
    let voices = tokio::task::spawn_blocking(move || voices_in(&models)).await.unwrap_or_else(|_| Err("the voices could not be listed".to_string()));
    let body = match voices {
        Ok(list) => json!({ "voice": choice.voice, "lang": choice.lang, "default_voice": DEFAULT_VOICE, "default_lang": DEFAULT_LANG, "voices": list, "english_voices": list.iter().filter(|v| lang_of(v).is_some()).collect::<Vec<_>>() }),
        Err(why) => json!({ "voice": choice.voice, "lang": choice.lang, "default_voice": DEFAULT_VOICE, "default_lang": DEFAULT_LANG, "voices": null, "voices_error": why }),
    };
    Json(body).into_response()
}

#[derive(Deserialize)]
struct ChoiceRequest {
    /// The voice to use, or `null` for the default. The field must be there: a body without it (a misspelt name) is refused, and does not silently reset the choice.
    voice: serde_json::Value,
}

/// Keeps the voice to speak with (an English voice the speech engine has), or goes back to the default with `null`. Needs the UI token.
async fn put_choice(axum::Extension(scope): axum::Extension<Scope>, axum::Extension(dirs): axum::Extension<Dirs>, Json(body): Json<ChoiceRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let bad = |status: StatusCode, why: String| (status, Json(json!({ "error": why }))).into_response();
    let voice = match body.voice {
        serde_json::Value::Null => None,
        serde_json::Value::String(name) => Some(name.trim().to_string()),
        other => return bad(StatusCode::BAD_REQUEST, format!("voice is the name of a voice or null, not {other}")),
    };
    if let Some(name) = voice.as_deref() {
        if lang_of(name).is_none() {
            return bad(StatusCode::BAD_REQUEST, format!("{name:?} is not the name of an English voice (an accent letter a or b, f or m, an underscore and a name: af_nicole)"));
        }
        let models = dirs.models();
        match tokio::task::spawn_blocking(move || voices_in(&models)).await {
            Ok(Ok(list)) if list.iter().any(|v| v == name) => {}
            Ok(Ok(_)) => return bad(StatusCode::BAD_REQUEST, format!("the speech engine has no voice called {name:?}; GET this route for the voices it has")),
            Ok(Err(why)) => return bad(StatusCode::SERVICE_UNAVAILABLE, format!("the voice cannot be checked, so it was not changed: {why}")),
            Err(_) => return bad(StatusCode::INTERNAL_SERVER_ERROR, "the voices could not be listed".to_string()),
        }
    }
    let data = dirs.data();
    match save_to(&data, voice.as_deref()) {
        Ok(choice) => {
            if dirs.data.is_none() {
                *CURRENT.lock().unwrap_or_else(|p| p.into_inner()) = Some(choice.clone());
            }
            Json(json!({ "voice": choice.voice, "lang": choice.lang, "default": voice.is_none() })).into_response()
        }
        Err(why) => bad(StatusCode::INTERNAL_SERVER_ERROR, why),
    }
}

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    routes_in(Dirs::default())
}

/// The routes over folders of their own (a test's).
pub fn routes_in<S: Clone + Send + Sync + 'static>(dirs: Dirs) -> Router<S> {
    Router::new().route("/api/v1/voice/choice", get(get_choice).put(put_choice)).layer(axum::Extension(dirs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, Request};
    use http_body_util::BodyExt;
    use std::io::Write;
    use tower::ServiceExt;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_voice_choice_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A file of voices like the engine's: a zip with one entry a voice.
    fn voices_file(models: &Path, names: &[&str]) {
        std::fs::create_dir_all(models).unwrap();
        let file = std::fs::File::create(models.join("voices-v1.0.bin")).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for n in names {
            zip.start_file(format!("{n}.npy"), zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
            zip.write_all(b"not really an array").unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn an_english_voice_name_is_recognised_and_gives_its_accent_and_nothing_else_does() {
        for (name, lang) in [("af_sarah", "en-us"), ("am_adam", "en-us"), ("bf_emma", "en-gb"), ("bm_george", "en-gb"), ("af_nicole", "en-us")] {
            assert!(valid_voice_name(name), "{name}");
            assert_eq!(lang_of(name), Some(lang), "{name}");
        }
        // Other languages' voices, other shapes, and anything that could be a path or a flag.
        for bad in ["hf_alpha", "jf_alpha", "zf_xiaobei", "ef_dora", "af_", "af", "AF_SARAH", "af_sarah.npy", "../af_sarah", "af_sa rah", "af_sarah\n", "-af_sarah", "xf_sarah", "ag_sarah", "", "af_ünï"] {
            assert!(!valid_voice_name(bad), "{bad:?}");
            assert_eq!(lang_of(bad), None);
        }
        assert!(!valid_voice_name(&format!("af_{}", "a".repeat(21))), "a name of more than 20 characters");
    }

    #[test]
    fn a_choice_is_kept_in_a_file_read_back_and_forgotten_and_a_damaged_file_is_the_default() {
        let dir = temp("file");
        assert_eq!(load_from(&dir), Choice::default_choice(), "nothing kept: the default");
        let saved = save_to(&dir, Some("bf_emma")).unwrap();
        assert_eq!(saved, Choice { voice: "bf_emma".into(), lang: "en-gb".into() });
        assert_eq!(load_from(&dir), saved);
        assert!(!dir.join("voice_choice.json.part").exists(), "no half-written file is left");
        assert!(save_to(&dir, Some("../evil")).is_err() && save_to(&dir, Some("jf_alpha")).is_err());
        assert_eq!(load_from(&dir), saved, "a refused choice changes nothing");
        assert_eq!(save_to(&dir, None).unwrap(), Choice::default_choice());
        assert_eq!(load_from(&dir), Choice::default_choice());
        assert!(save_to(&dir, None).is_ok(), "forgetting what is not kept is fine");
        for damaged in ["not json", "{\"voice\": 5}", "{\"voice\": \"../../x\"}", "{\"voice\": \"jf_alpha\"}", "{}", "\u{feff}{\"voice\": \"am_adam\"}"] {
            std::fs::write(dir.join("voice_choice.json"), damaged).unwrap();
            let got = load_from(&dir);
            if damaged.contains("am_adam") {
                assert_eq!(got.voice, "am_adam", "a byte-order mark is allowed");
            } else {
                assert_eq!(got, Choice::default_choice(), "{damaged:?}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_voices_are_the_names_inside_the_voices_file_and_a_missing_or_broken_file_is_a_reason() {
        let models = temp("voices").join("kokoro");
        assert!(voices_in(&models).unwrap_err().contains("voices file is not in"));
        voices_file(&models, &["bm_george", "af_sarah", "af_nicole", "zf_xiaobei"]);
        assert_eq!(voices_in(&models).unwrap(), vec!["af_nicole", "af_sarah", "bm_george", "zf_xiaobei"]);
        std::fs::write(models.join("voices-v1.0.bin"), b"this is not a zip").unwrap();
        assert!(voices_in(&models).unwrap_err().contains("not a file of voices"));
        let _ = std::fs::remove_dir_all(models.parent().unwrap());
    }

    async fn send(dirs: &Dirs, method: Method, scope: Scope, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let app = routes_in::<()>(dirs.clone()).layer(axum::Extension(scope));
        let mut req = Request::builder().method(method).uri("/api/v1/voice/choice");
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[test]
    fn the_route_lists_the_voices_keeps_an_english_one_the_engine_has_and_refuses_the_rest() {
        let base = temp("route");
        let (data, models) = (base.join("data"), base.join("kokoro"));
        voices_file(&models, &["af_sarah", "af_nicole", "bm_george", "zf_xiaobei"]);
        let dirs = Dirs::at(data.clone(), models.clone());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let (status, body) = rt.block_on(send(&dirs, Method::GET, Scope::Api, None));
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!((body["voice"].as_str(), body["lang"].as_str(), body["default_voice"].as_str()), (Some("af_sarah"), Some("en-us"), Some("af_sarah")));
        assert_eq!(body["voices"], json!(["af_nicole", "af_sarah", "bm_george", "zf_xiaobei"]));
        assert_eq!(body["english_voices"], json!(["af_nicole", "af_sarah", "bm_george"]), "the others need a language setting");

        // The API token may look and not change.
        let (status, _) = rt.block_on(send(&dirs, Method::PUT, Scope::Api, Some(json!({"voice": "bm_george"}))));
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(load_from(&data), Choice::default_choice());

        // The UI token keeps a voice the engine has, and the language follows the voice.
        let (status, body) = rt.block_on(send(&dirs, Method::PUT, Scope::Ui, Some(json!({"voice": "bm_george"}))));
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!((body["voice"].as_str(), body["lang"].as_str()), (Some("bm_george"), Some("en-gb")));
        assert_eq!(load_from(&data).voice, "bm_george");
        let (_, body) = rt.block_on(send(&dirs, Method::GET, Scope::Ui, None));
        assert_eq!(body["voice"], "bm_george");

        // A voice the engine does not have, another language's voice, a name that is a path, and a body without a voice change nothing.
        for (bad, status) in [("af_missing", StatusCode::BAD_REQUEST), ("zf_xiaobei", StatusCode::BAD_REQUEST), ("../../evil", StatusCode::BAD_REQUEST)] {
            let (got, body) = rt.block_on(send(&dirs, Method::PUT, Scope::Ui, Some(json!({"voice": bad}))));
            assert_eq!(got, status, "{bad}: {body}");
            assert!(body["error"].is_string());
            assert_eq!(load_from(&data).voice, "bm_george", "{bad}");
        }

        let (got, _) = rt.block_on(send(&dirs, Method::PUT, Scope::Ui, Some(json!({}))));
        assert_eq!(got, StatusCode::UNPROCESSABLE_ENTITY, "a body with no voice is not a reset");
        let (got, _) = rt.block_on(send(&dirs, Method::PUT, Scope::Ui, Some(json!({"voice": 5}))));
        assert_eq!(got, StatusCode::BAD_REQUEST);
        assert_eq!(load_from(&data).voice, "bm_george");

        // `null` goes back to the default.
        let (status, body) = rt.block_on(send(&dirs, Method::PUT, Scope::Ui, Some(json!({"voice": null}))));
        assert_eq!((status, body["voice"].as_str(), body["default"].as_bool()), (StatusCode::OK, Some("af_sarah"), Some(true)), "{body}");
        assert!(!data.join("voice_choice.json").exists());

        // While the voices cannot be read nothing is changed (a wrong name would silence every answer), but a reset still works.
        std::fs::remove_file(models.join("voices-v1.0.bin")).unwrap();
        let (status, body) = rt.block_on(send(&dirs, Method::PUT, Scope::Ui, Some(json!({"voice": "af_nicole"}))));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(!data.join("voice_choice.json").exists());
        let (status, body) = rt.block_on(send(&dirs, Method::GET, Scope::Ui, None));
        assert_eq!(status, StatusCode::OK);
        assert!(body["voices"].is_null() && body["voices_error"].is_string(), "{body}");
        let (status, _) = rt.block_on(send(&dirs, Method::PUT, Scope::Ui, Some(json!({"voice": null}))));
        assert_eq!(status, StatusCode::OK);
        let _ = std::fs::remove_dir_all(&base);
    }
}
