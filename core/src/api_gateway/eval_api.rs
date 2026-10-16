//! `GET /api/v1/eval/scoreboard`: the latest AquaBench scoreboard, as `scripts/eval/scoreboard.py` wrote it (card R7-04, `scripts/eval/README.md`).
//!
//! It only reads: the file `scripts/eval/out/scoreboard.json` under the workspace root, parsed and handed on. A file that is not there, is not a JSON object, is a link or a folder,
//! cannot be read, or is larger than 1 MB is answered with `{"present": false, "hint": "..."}` and status 200, never a 500: a screen shows the hint instead of an error. Nothing is
//! written, run or started, and the route needs either token (the scoreboard has task names and pass rates, no secrets and no paths of the person's).
//!
//! The route takes [`EvalState`], the workspace root, and not the whole gateway state, so that a test builds the router over a folder of its own.

use super::server::AppState;
use axum::{
    extract::{FromRef, State},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The most of the file that is read and served. The scoreboard of a full run is a few tens of kilobytes; a larger file is not the scoreboard.
pub const MAX_BYTES: u64 = 1 << 20;

/// What the route uses: the workspace root the scoreboard is looked for under. The gateway's [`AppState`] gives it (`FromRef`), read at each request.
#[derive(Clone)]
pub struct EvalState {
    pub root: PathBuf,
}

impl FromRef<AppState> for EvalState {
    fn from_ref(_app: &AppState) -> Self {
        Self { root: crate::runtime::workspace_root() }
    }
}

/// Where `scoreboard.py` writes the scoreboard by default, under a workspace root.
pub fn scoreboard_path(root: &Path) -> PathBuf {
    root.join("scripts").join("eval").join("out").join("scoreboard.json")
}

/// The same place as the hint names it (relative, so the answer holds no path of this computer).
const SHOWN: &str = "scripts/eval/out/scoreboard.json";

fn absent(why: impl std::fmt::Display) -> Value {
    json!({ "present": false, "hint": format!("{why}. Run `python scripts/eval/scoreboard.py --report <report.json>` to make it (see scripts/eval/README.md).") })
}

/// The scoreboard under `root`: the file's JSON object with `"present": true` added, or `{"present": false, "hint": ...}` for a file that is missing or cannot be served. Reads at most
/// [`MAX_BYTES`] and never writes.
pub fn read_scoreboard(root: &Path) -> Value {
    let path = scoreboard_path(root);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_file() => {}
        Ok(_) => return absent(format!("{SHOWN} is not a plain file, so it is not served")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return absent(format!("There is no scoreboard yet ({SHOWN} is missing)")),
        Err(_) => return absent(format!("{SHOWN} cannot be looked at")),
    }
    let mut bytes = Vec::new();
    let read = std::fs::File::open(&path).and_then(|f| f.take(MAX_BYTES + 1).read_to_end(&mut bytes));
    if read.is_err() {
        return absent(format!("{SHOWN} cannot be read"));
    }
    if bytes.len() as u64 > MAX_BYTES {
        return absent(format!("{SHOWN} is larger than 1 MB, so it is not served"));
    }
    let text = String::from_utf8_lossy(&bytes);
    match serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
        Ok(Value::Object(mut board)) => {
            board.insert("present".into(), json!(true));
            Value::Object(board)
        }
        Ok(_) => absent(format!("{SHOWN} is not a JSON object")),
        Err(e) => absent(format!("{SHOWN} is not valid JSON ({e})")),
    }
}

/// The route.
pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    EvalState: FromRef<S>,
{
    Router::new().route("/api/v1/eval/scoreboard", get(get_scoreboard))
}

/// Gives the latest AquaBench scoreboard, as `scoreboard.py` wrote it, with `present` true, or `present` false and a `hint` when there is none to serve. It only reads a file and writes nothing; either token.
async fn get_scoreboard(State(state): State<EvalState>) -> Response {
    let board = tokio::task::spawn_blocking(move || read_scoreboard(&state.root)).await.unwrap_or_else(|_| absent("The scoreboard could not be read just now"));
    Json(board).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// A new workspace root for one test, with or without the scoreboard in it.
    fn root(name: &str, board: Option<&[u8]>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua_eval_api_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(bytes) = board {
            let path = scoreboard_path(&dir);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        dir
    }

    fn hint_of(v: &Value) -> String {
        assert_eq!(v["present"], false, "{v}");
        v["hint"].as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn a_scoreboard_that_is_there_is_served_as_it_was_written_with_present_true() {
        let board = json!({ "schema": 1, "generated": "2026-10-08T21:00:00+00:00", "overall": { "tasks_run": 12, "pass_at_1": 0.75 }, "categories": { "self_repair": { "pass_at_1": 1.0 } }, "regressions": [] });
        let dir = root("served", Some(board.to_string().as_bytes()));
        let got = read_scoreboard(&dir);
        assert_eq!(got["present"], true);
        assert_eq!((got["schema"].clone(), got["overall"]["pass_at_1"].clone(), got["categories"]["self_repair"]["pass_at_1"].clone()), (json!(1), json!(0.75), json!(1.0)), "{got}");
        // A byte order mark in front (a file written by a Windows editor) is no reason to refuse it.
        let mut with_mark = vec![0xEF, 0xBB, 0xBF];
        with_mark.extend_from_slice(board.to_string().as_bytes());
        assert_eq!(read_scoreboard(&root("served_bom", Some(&with_mark)))["present"], true);
        // Reading changed nothing.
        assert_eq!(std::fs::read(scoreboard_path(&dir)).unwrap(), board.to_string().as_bytes());
    }

    #[test]
    fn a_scoreboard_that_is_missing_or_cannot_be_served_is_a_hint_and_never_an_error() {
        // Missing: the hint says how to make one, and holds no path of this computer.
        let dir = root("missing", None);
        let hint = hint_of(&read_scoreboard(&dir));
        assert!(hint.contains("no scoreboard yet") && hint.contains("scripts/eval/scoreboard.py"), "{hint}");
        assert!(!hint.contains(&dir.to_string_lossy().to_string()), "{hint}");
        // Not JSON, not an object, empty.
        assert!(hint_of(&read_scoreboard(&root("not_json", Some(b"{ this is not json")))).contains("not valid JSON"));
        assert!(hint_of(&read_scoreboard(&root("array", Some(b"[1, 2, 3]")))).contains("not a JSON object"));
        assert!(hint_of(&read_scoreboard(&root("empty", Some(b"")))).contains("not valid JSON"));
        // A folder in its place.
        let folder = root("folder", None);
        std::fs::create_dir_all(scoreboard_path(&folder)).unwrap();
        assert!(hint_of(&read_scoreboard(&folder)).contains("not a plain file"));
        // Larger than 1 MB: a valid object, padded; it is refused, not read in full. Exactly 1 MB is still served.
        let pad = |total: u64| {
            let mut bytes = b"{\"overall\": {}}".to_vec();
            bytes.resize(total as usize, b' ');
            bytes
        };
        let big = root("big", Some(&pad(MAX_BYTES + 1)));
        assert!(hint_of(&read_scoreboard(&big)).contains("larger than 1 MB"));
        assert_eq!(read_scoreboard(&root("exactly", Some(&pad(MAX_BYTES))))["present"], true);
        // The workspace root itself not being there is the same as no scoreboard.
        assert!(hint_of(&read_scoreboard(&dir.join("no_such_workspace"))).contains("no scoreboard yet"));
    }

    /// The route over a folder of its own: (status, body).
    fn get_route(state: &EvalState) -> (StatusCode, Value) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let app = routes::<EvalState>().with_state(state.clone());
        rt.block_on(async {
            let resp = app.oneshot(Request::builder().uri("/api/v1/eval/scoreboard").body(Body::empty()).unwrap()).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        })
    }

    #[test]
    fn the_route_answers_200_with_the_scoreboard_or_with_a_hint_and_only_to_a_get() {
        let there = EvalState { root: root("route_there", Some(json!({ "overall": { "tasks_run": 3 }, "regressions": [] }).to_string().as_bytes())) };
        let (status, body) = get_route(&there);
        assert_eq!((status, body["present"].clone(), body["overall"]["tasks_run"].clone()), (StatusCode::OK, json!(true), json!(3)), "{body}");
        let missing = EvalState { root: root("route_missing", None) };
        let (status, body) = get_route(&missing);
        assert_eq!(status, StatusCode::OK, "a missing scoreboard is not an error");
        assert!(hint_of(&body).contains("no scoreboard yet"));
        // Nothing but a GET: the route changes nothing, so a POST is not served.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let app = routes::<EvalState>().with_state(there.clone());
        let status = rt.block_on(async { app.oneshot(Request::builder().method("POST").uri("/api/v1/eval/scoreboard").body(Body::empty()).unwrap()).await.unwrap().status() });
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(scoreboard_path(&there.root).exists(), "and the file is as it was");
    }
}
