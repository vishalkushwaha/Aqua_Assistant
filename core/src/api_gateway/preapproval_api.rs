//! Pre-approved project checks over HTTP (card OA10-05; task WF-14 of the OpenCode audit; decision D-07 taken as the audit proposes and shipped off).
//!
//! The person allows, once and with the app's token, the tests and the build of one folder for one run, so that a run that is started from a script (`aqua-cli run --preapproval`) does not
//! stop at each of them. What a pre-approval is and what it covers is in `aqua_tool_host::preapproval`; this file is the doors:
//!
//! * `POST /api/v1/preapprovals` `{ folder, kinds?, budget?, minutes? }` makes one: **the UI token only** (the API token is refused, as it is for an answer to a question: a script cannot
//!   make the permission it then uses), and only while the core runs with `AQUA_PREAPPROVALS=on` (`409` otherwise). The folder must be one Aqua may use already; nothing is added to the
//!   allowed folders by this route. The answer carries the id that is given to a run (`POST /api/v1/runs` with `preapproval_id`), and whether the folder is a trusted one (a check in a
//!   folder that is not trusted asks as it always did).
//! * `GET /api/v1/preapprovals` lists them with how much of each is left (UI token only).
//! * `DELETE /api/v1/preapprovals/{id}` takes one back: the next check of its run asks again (UI token only).
//!
//! A pre-approval is kept in memory: a restart of Aqua forgets every one.

use super::auth::{ui_scope_required, Scope};
use super::server::api_error;
use aqua_native_tools::fs::guard::display;
use aqua_native_tools::fs::Op;
use aqua_tool_host::preapproval::{self, View};
use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/preapprovals", post(preapproval_create).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)).get(preapproval_list))
        .route("/api/v1/preapprovals/:id", axum::routing::delete(preapproval_revoke))
}

/// The body of `POST /api/v1/preapprovals`.
#[derive(Deserialize, Default, Debug)]
pub struct CreateRequest {
    /// The folder the checks may run in (and the run works in).
    pub folder: String,
    /// Which checks: some of `tests`, `build`, `lint`, `typecheck` (`tests` when left out).
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// How many calls it answers at most (60 by default, 500 at most).
    #[serde(default)]
    pub budget: Option<u32>,
    /// How long it lasts, in minutes (30 by default, 720 at most).
    #[serde(default)]
    pub minutes: Option<u32>,
}

/// A pre-approval as the person sees it.
pub fn view_json(v: &View) -> Value {
    json!({
        "id": v.id,
        "folder": v.folder,
        "kinds": v.kinds,
        "budget": v.budget,
        "used": v.used,
        "minutes": v.minutes,
        "expires_in_secs": v.expires_in_secs,
        "run_id": v.run_id,
        "state": v.state.as_str(),
    })
}

fn host() -> Result<&'static std::sync::Arc<aqua_tool_host::ToolHost>, Response> {
    crate::agent_tools::host().ok_or_else(|| api_error(StatusCode::SERVICE_UNAVAILABLE, "the tool host is not running, so nothing can be pre-approved"))
}

#[cfg(test)]
thread_local! {
    /// The file guard a test gives this thread in place of the process's (the process has one, set once at start-up).
    pub(crate) static TEST_GUARD: std::cell::RefCell<Option<std::sync::Arc<aqua_native_tools::fs::PathGuard>>> = const { std::cell::RefCell::new(None) };
}

/// The file guard that decides which folders Aqua may use: the process's, or (in a test) the one the test gave this thread.
pub(crate) fn the_guard() -> Option<std::sync::Arc<aqua_native_tools::fs::PathGuard>> {
    #[cfg(test)]
    if let Some(own) = TEST_GUARD.with(|g| g.borrow().clone()) {
        return Some(own);
    }
    crate::tool_host::project_guard()
}

/// The folder as the file guard resolves it (and whether it trusts it): the same folder a run would be given. Never adds a folder to the allowed ones.
pub(crate) fn resolve_folder(folder: &str) -> Result<(String, bool), Response> {
    let Some(guard) = the_guard() else {
        return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, "the file tools are not running, so no folder can be named"));
    };
    match guard.resolve(folder, Op::Read) {
        Ok(r) if r.path.is_dir() => Ok((display(&r.path), r.trusted)),
        Ok(_) => Err(api_error(StatusCode::BAD_REQUEST, "that is not a folder")),
        Err(_) => Err(api_error(StatusCode::FORBIDDEN, "that folder is not one Aqua may use: allow it in the app first (nothing is added to the allowed folders by this request)")),
    }
}

/// Makes a pre-approval for one folder (the UI token only, and only with `AQUA_PREAPPROVALS=on`).
async fn preapproval_create(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<CreateRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if !preapproval::enabled() {
        return api_error(StatusCode::CONFLICT, "pre-approvals are off: start the core with AQUA_PREAPPROVALS=on");
    }
    let host = match host() {
        Ok(h) => h,
        Err(r) => return r,
    };
    let (folder, trusted) = match resolve_folder(&body.folder) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let kinds = body.kinds.unwrap_or_else(|| vec!["tests".to_string()]);
    match host.preapprovals().create(&folder, &kinds, body.budget, body.minutes) {
        Ok(v) => {
            let mut out = view_json(&v);
            out["trusted"] = json!(trusted);
            (StatusCode::OK, Json(out)).into_response()
        }
        Err(why) => api_error(StatusCode::BAD_REQUEST, &why),
    }
}

/// The pre-approvals there are, with what is left of each (the UI token only).
async fn preapproval_list(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let host = match host() {
        Ok(h) => h,
        Err(r) => return r,
    };
    Json(json!({ "enabled": preapproval::enabled(), "preapprovals": host.preapprovals().list().iter().map(view_json).collect::<Vec<_>>() })).into_response()
}

/// Takes a pre-approval back: the next check of the run it was given to asks again (the UI token only).
async fn preapproval_revoke(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let host = match host() {
        Ok(h) => h,
        Err(r) => return r,
    };
    if host.preapprovals().revoke(&id) {
        Json(json!({ "id": id, "revoked": true })).into_response()
    } else {
        api_error(StatusCode::NOT_FOUND, "there is no such pre-approval (it was taken back, it ended, or Aqua was restarted)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvGuard;
    use aqua_native_tools::fs::{FsConfig, PathGuard};
    use aqua_tool_host::{AuditLedger, Mode, PolicyEngine, ToolHost};
    use std::sync::Arc;

    /// A tool host and a file guard of the test's own, as the handlers of this thread see them.
    struct World {
        host: Arc<ToolHost>,
        dir: std::path::PathBuf,
    }

    fn world(name: &str) -> World {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("scratch_preapproval_api").join(format!("{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws/project")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::fs::create_dir_all(dir.join("aqua")).unwrap();
        // The folder as the system spells it (the guard refuses a path with `..` in it).
        let dir = std::path::PathBuf::from(std::fs::canonicalize(&dir).unwrap().to_string_lossy().trim_start_matches(r"\\?\"));
        let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("aqua/audit.jsonl")))));
        World { host, dir }
    }

    /// Runs `f` with this world's host and guard for the handlers that run on this thread.
    fn within<F: std::future::Future>(w: &World, f: F) -> F::Output {
        let guard = Arc::new(PathGuard::new(FsConfig::with_workspace(w.dir.join("ws").to_string_lossy().to_string()), vec![w.dir.join("aqua")]).unwrap());
        TEST_GUARD.with(|g| *g.borrow_mut() = Some(guard));
        let out = crate::agent_tools::with_host(w.host.clone(), || tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f));
        TEST_GUARD.with(|g| *g.borrow_mut() = None);
        out
    }

    async fn read(r: Response) -> (StatusCode, Value) {
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn body(folder: &std::path::Path) -> CreateRequest {
        CreateRequest { folder: folder.to_string_lossy().to_string(), kinds: Some(vec!["tests".into(), "build".into()]), budget: Some(5), minutes: Some(15) }
    }

    #[test]
    fn a_script_with_the_api_token_cannot_make_list_or_take_back_a_pre_approval_not_even_with_the_switch_on() {
        let _on = EnvGuard::set(&[("AQUA_PREAPPROVALS", "on")]);
        let w = world("api_token");
        within(&w, async {
            // The person makes one (so that there is something to list and to take back).
            let (code, made) = read(preapproval_create(axum::Extension(Scope::Ui), Json(body(&w.dir.join("ws/project")))).await).await;
            assert_eq!(code, StatusCode::OK, "{made}");
            let id = made["id"].as_str().unwrap().to_string();
            for (what, response) in [
                ("make", preapproval_create(axum::Extension(Scope::Api), Json(body(&w.dir.join("ws/project")))).await),
                ("list", preapproval_list(axum::Extension(Scope::Api)).await),
                ("take back", preapproval_revoke(axum::Extension(Scope::Api), Path(id.clone())).await),
            ] {
                let (code, answer) = read(response).await;
                assert_eq!((code, answer["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden_scope")), "{what}: {answer}");
            }
            // Nothing was made, shown or taken back by them.
            assert_eq!(w.host.preapprovals().list().len(), 1, "the one the person made is still the only one");
            assert!(w.host.preapprovals().list()[0].id == id);
        });
    }

    #[test]
    fn it_is_off_unless_the_switch_says_on_and_a_folder_aqua_may_not_use_is_refused() {
        let w = world("off_and_folders");
        {
            // The switch is not set: the guard holds the lock and makes sure of it.
            let _off = EnvGuard::hold();
            std::env::remove_var("AQUA_PREAPPROVALS");
            within(&w, async {
                let (code, answer) = read(preapproval_create(axum::Extension(Scope::Ui), Json(body(&w.dir.join("ws/project")))).await).await;
                assert_eq!(code, StatusCode::CONFLICT, "the switch is off: {answer}");
                assert!(answer["error"].as_str().unwrap().contains("AQUA_PREAPPROVALS=on"), "{answer}");
                assert!(w.host.preapprovals().list().is_empty());
            });
        }
        let _on = EnvGuard::set(&[("AQUA_PREAPPROVALS", "on")]);
        within(&w, async {
            let (code, answer) = read(preapproval_create(axum::Extension(Scope::Ui), Json(body(&w.dir.join("elsewhere")))).await).await;
            assert_eq!(code, StatusCode::FORBIDDEN, "{answer}");
            assert!(answer["error"].as_str().unwrap().contains("nothing is added to the allowed folders"), "{answer}");
            let (code, _) = read(preapproval_create(axum::Extension(Scope::Ui), Json(CreateRequest { kinds: Some(vec!["deploy".into()]), ..body(&w.dir.join("ws/project")) })).await).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "a kind that is not one");
            let (code, _) = read(preapproval_create(axum::Extension(Scope::Ui), Json(CreateRequest { budget: Some(100_000), ..body(&w.dir.join("ws/project")) })).await).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "a number that cannot be given");
            assert!(w.host.preapprovals().list().is_empty(), "none of them made one");
        });
    }

    #[test]
    fn the_person_makes_lists_and_takes_back_one_and_the_answer_says_whether_the_folder_is_trusted() {
        let _on = EnvGuard::set(&[("AQUA_PREAPPROVALS", "on")]);
        let w = world("make_list_revoke");
        within(&w, async {
            let (code, made) = read(preapproval_create(axum::Extension(Scope::Ui), Json(body(&w.dir.join("ws/project")))).await).await;
            assert_eq!(code, StatusCode::OK, "{made}");
            assert_eq!((made["kinds"].clone(), made["budget"].as_u64(), made["minutes"].as_u64(), made["trusted"].as_bool(), made["state"].as_str(), made["used"].as_u64()), (json!(["build", "tests"]), Some(5), Some(15), Some(true), Some("open"), Some(0)));
            assert!(made["id"].as_str().unwrap().starts_with("pa_") && made["run_id"].is_null());
            let (code, listed) = read(preapproval_list(axum::Extension(Scope::Ui)).await).await;
            assert_eq!((code, listed["enabled"].clone(), listed["preapprovals"].as_array().map(Vec::len)), (StatusCode::OK, json!(true), Some(1)), "{listed}");
            let id = made["id"].as_str().unwrap().to_string();
            let (code, gone) = read(preapproval_revoke(axum::Extension(Scope::Ui), Path(id.clone())).await).await;
            assert_eq!((code, gone["revoked"].clone()), (StatusCode::OK, json!(true)), "{gone}");
            assert_eq!(read(preapproval_revoke(axum::Extension(Scope::Ui), Path(id)).await).await.0, StatusCode::NOT_FOUND);
            assert!(w.host.preapprovals().list().is_empty());
        });
    }

    #[test]
    fn a_run_is_started_with_a_pre_approval_only_in_its_folder_once_and_only_while_the_switch_is_on() {
        use super::super::run_control_api::{start_run, StartRequest};
        use super::super::run_host::{self, Work};
        use crate::modules::browser_agent::McpAgentResult;
        let _on = EnvGuard::set(&[("AQUA_PREAPPROVALS", "on")]);
        let w = world("start_with");
        let ended = || -> run_host::MakeWork {
            Arc::new(|_goal: &str| -> Work { Box::new(|| Ok(McpAgentResult { final_answer: "Done.".into(), steps: vec![], tool_rounds: 0, stopped_reason: "final".into(), gate: None, run_id: Some("run_not_recorded".into()), question: None, choices: Vec::new(), sources: Vec::new() })) })
        };
        let project = w.dir.join("ws/project").to_string_lossy().to_string();
        within(&w, async {
            let (code, made) = read(preapproval_create(axum::Extension(Scope::Ui), Json(body(&w.dir.join("ws/project")))).await).await;
            assert_eq!(code, StatusCode::OK, "{made}");
            let id = made["id"].as_str().unwrap().to_string();
            let start = |directory: Option<&str>, pre: &str| StartRequest { goal: "fix the failing test".into(), directory: directory.map(str::to_string), preapproval_id: Some(pre.into()), ..Default::default() };
            // A pre-approval is for one folder: a start without one, one outside it, an id that is nobody's: refused, and the pre-approval is still whole.
            let (code, answer) = read(start_run(Scope::Api, start(None, &id), |_| ended())).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{answer}");
            let (code, answer) = read(start_run(Scope::Api, start(Some(&w.dir.join("ws").to_string_lossy()), &id), |_| ended())).await;
            assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "the run works outside the folder: {answer}");
            let (code, _) = read(start_run(Scope::Api, start(Some(&project), "pa_nobody"), |_| ended())).await;
            assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(w.host.preapprovals().list()[0].run_id, None);
            // The API token may name it (it makes nothing: the person did), and the run it starts has it.
            let (code, started) = read(start_run(Scope::Api, start(Some(&project), &id), |_| ended())).await;
            assert_eq!(code, StatusCode::ACCEPTED, "{started}");
            let run_id = started["run_id"].as_str().unwrap().to_string();
            assert_eq!(w.host.preapprovals().list()[0].run_id.as_deref(), Some(run_id.as_str()), "it was given to that run, by the id the run will be recorded under");
            // It covers one run: a second start with it is refused.
            let (code, again) = read(start_run(Scope::Api, start(Some(&project), &id), |_| ended())).await;
            assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{again}");
            // The run ends, and the pre-approval ends with it.
            for _ in 0..500 {
                if run_host::lookup(&run_id).is_some_and(|v| v.state == run_host::State::Ended) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(w.host.preapprovals().list().is_empty(), "the run ended: {:?}", w.host.preapprovals().list());
        });
        // With the switch off a start that names one is refused whole.
        let _off = EnvGuard::hold();
        std::env::remove_var("AQUA_PREAPPROVALS");
        within(&w, async {
            let (code, answer) = read(start_run(Scope::Api, StartRequest { goal: "do it".into(), directory: Some(project.clone()), preapproval_id: Some("pa_x".into()), ..Default::default() }, |_| ended())).await;
            assert_eq!(code, StatusCode::CONFLICT, "{answer}");
        });
    }
}
