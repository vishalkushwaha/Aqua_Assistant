//! Which language model answers which role, over HTTP (card OA9-03; task PR-05 of the OpenCode audit).
//!
//! `GET /api/v1/models/roles` says, for each role (agent, chat, edit, fast, decision, diagnosis, summary), the model it uses **and why** (`set_by_you`, `environment`,
//! `default_rule`, `active_model` or `follows_agent`), the models in the folder, a warning when two roles on different models make the server swap, and why `profiles.json` or
//! `router.json` was not used if it was not. `PUT /api/v1/models/roles` gives roles a model of the folder, or takes a choice away. The same table is in `GET /api/v1/models/status`
//! (`llm_roles`).
//!
//! Which model decides what Aqua does is part of what Aqua trusts, so the change needs the UI token, as the Use button's route does: a script with the API token can read the table and
//! cannot change it. Every change leaves a line in the audit ledger. Only a model of this machine can be named: another provider needs the remote policy, which is not built.

use super::auth::{ui_scope_required, Scope};
use super::server::{api_error, AppState};
use crate::modules::llm_router::Role;
use axum::{extract::State, http::StatusCode, response::{IntoResponse, Response}, routing::{get, post}, Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/models/roles", get(get_roles).put(put_roles).layer(axum::extract::DefaultBodyLimit::max(8 * 1024)))
        .route("/api/v1/models/providers", get(get_providers))
        .route("/api/v1/models/providers/test", post(post_provider_test).layer(axum::extract::DefaultBodyLimit::max(2 * 1024)))
}

/// The model servers on this machine that `providers.json` lists (card OA9-04): what each lists, the ones that are refused and why (an address of another machine always is), whether
/// the key each names is in the credential store (a yes or a no, never the key) and what the egress gate has decided lately. Off unless `AQUA_MODEL_PROVIDERS` is on, and then it says so.
/// Either token.
async fn get_providers(State(state): State<AppState>) -> Response {
    let registry = crate::model_providers::shared(&state.models_dir);
    // The switch is read here, on the thread of the request: the reading of the file happens on another.
    let enabled = crate::model_providers::enabled();
    match tokio::task::spawn_blocking(move || registry.status(enabled)).await {
        Ok(status) => Json(status).into_response(),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the list of model providers could not be made"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderTestRequest {
    /// A provider of `providers.json`.
    provider: String,
    /// One of the models it lists: with it, the test also makes one short call to it ("Reply with the single word OK."); without it, only the list of the server's models is asked for.
    #[serde(default)]
    model: Option<String>,
}

/// Tries a model server of `providers.json`: asks it for its models, tells which of them the file lists and which it does not, and, when a model is named, makes one short call to it
/// through the router and its gate (the words are fixed: this route cannot be made to send a text of the caller's). The UI token, because it opens a connection; a server on another
/// machine is never reached (400 with the reason).
async fn post_provider_test(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<ProviderTestRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let registry = crate::model_providers::shared(&state.models_dir);
    let summary = format!("{}{}", body.provider, body.model.as_deref().map(|m| format!("/{m}")).unwrap_or_default());
    let (provider, model) = (body.provider, body.model);
    let enabled = crate::model_providers::enabled();
    match tokio::task::spawn_blocking(move || registry.test(enabled, &provider, model.as_deref())).await {
        Ok(Ok(result)) => {
            aqua_tool_host::record_direct("ui", "models.providers.test", &summary, true, 0);
            Json(result).into_response()
        }
        Ok(Err(why)) => {
            aqua_tool_host::record_direct("ui", "models.providers.test", &summary, false, 0);
            api_error(StatusCode::BAD_REQUEST, &why)
        }
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the test of the model provider could not be made"),
    }
}

/// What the table of roles is now, as the answer of both methods.
fn table(state: &AppState) -> serde_json::Value {
    json!({
        "roles": state.llm.role_choices().iter().map(|c| c.view()).collect::<Vec<_>>(),
        "installed": state.llm.installed_filenames(),
        "warnings": state.llm.role_warnings(),
        "problems": state.llm.model_file_problems(),
        // Which speed-experiment switches are on and what the last launch of the model server did with them (card OA9-06). Everything in it is off by default.
        "speed_experiments": crate::modules::llm_router::server_options::report(state.llm.model_dir(), &state.llm.agent_model_filename(), &state.llm.installed_filenames(), state.llm.profiles()),
    })
}

/// The model each role uses and why, the models in the folder, and a warning when roles on different models make the server swap. Either token.
async fn get_roles(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(table(&state))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RolesRequest {
    /// Role name to a file of the model folder (`Qwen3-8B-Q4_K_M.gguf` or `llama.cpp/Qwen3-8B-Q4_K_M.gguf`); null or an empty text takes the choice away.
    roles: BTreeMap<String, Option<String>>,
}

/// Gives roles a model of the model folder, or takes a choice away (null): all the changes are made or none. The UI token.
async fn put_roles(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<RolesRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let mut changes: Vec<(Role, Option<String>)> = Vec::new();
    for (name, model) in &body.roles {
        let Some(role) = Role::parse(name) else {
            let all: Vec<&str> = Role::ALL.iter().map(|r| r.name()).collect();
            return api_error(StatusCode::BAD_REQUEST, &format!("{name:?} is not a role (the roles are {})", all.join(", ")));
        };
        changes.push((role, model.clone().filter(|m| !m.trim().is_empty())));
    }
    let summary = changes.iter().map(|(r, m)| format!("{}={}", r.name(), m.as_deref().unwrap_or("(default)"))).collect::<Vec<_>>().join(", ");
    match state.llm.assign_roles(&changes) {
        Ok(_) => {
            aqua_tool_host::record_direct("ui", "models.roles", &summary, true, 0);
            Json(table(&state)).into_response()
        }
        Err(e) => {
            aqua_tool_host::record_direct("ui", "models.roles", &summary, false, 0);
            api_error(StatusCode::BAD_REQUEST, &e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_gateway::auth::{require_local_auth, ApiAuth};
    use crate::api_gateway::server::api_routes;
    use crate::test_support::{app_state, TempWorld};
    use aqua_tool_host::{AuditLedger, Mode, PolicyEngine, ToolHost};
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use std::sync::Arc;
    use tower::ServiceExt;

    const API: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const UI: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A model folder with these files in it (each only has to be a file: the roles do not read a model).
    fn world_with(files: &[&str]) -> (TempWorld, axum::Router) {
        let world = TempWorld::new("models_api");
        for f in files {
            world.write(&format!("models/llm/{f}"), "GGUF not a model, only a file");
        }
        let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(&world.path("host.audit.jsonl")))));
        let state = app_state(&world, host);
        let auth = ApiAuth::new(API.to_string(), 8767).with_ui_token(UI.to_string());
        let app = api_routes().layer(axum::middleware::from_fn_with_state(auth, require_local_auth)).with_state(state);
        (world, app)
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    async fn call(app: &axum::Router, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path).header("host", "127.0.0.1:8767");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let request = match body {
            Some(b) => request.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body()).await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn model_of(table: &Value, role: &str) -> (String, String) {
        let r = table["roles"].as_array().unwrap().iter().find(|r| r["role"] == role).unwrap_or_else(|| panic!("no {role} in {table}"));
        (r["model"].as_str().unwrap().to_string(), r["source"].as_str().unwrap().to_string())
    }

    #[test]
    fn the_table_is_read_with_either_token_and_changed_with_the_ui_token_only() {
        let (_world, app) = world_with(&["Qwen3-8B-Q4_K_M.gguf", "Llama-3.2-3B-Q4_K_M.gguf"]);
        run(async {
            for token in [API, UI] {
                let (status, table) = call(&app, "GET", "/api/v1/models/roles", Some(token), None).await;
                assert_eq!(status, StatusCode::OK, "{table}");
                assert_eq!(table["roles"].as_array().unwrap().len(), 7);
                assert_eq!(model_of(&table, "agent"), ("Qwen3-8B-Q4_K_M.gguf".to_string(), "default_rule".to_string()));
                assert_eq!(model_of(&table, "chat"), ("Qwen3-8B-Q4_K_M.gguf".to_string(), "follows_agent".to_string()));
                assert_eq!(table["installed"].as_array().unwrap().len(), 2);
                assert!(table["warnings"].as_array().unwrap().is_empty());
            }
            assert_eq!(call(&app, "GET", "/api/v1/models/roles", None, None).await.0, StatusCode::UNAUTHORIZED);
            let change = json!({"roles": {"chat": "Llama-3.2-3B-Q4_K_M.gguf"}});
            assert_eq!(call(&app, "PUT", "/api/v1/models/roles", None, Some(change.clone())).await.0, StatusCode::UNAUTHORIZED);
            let (status, body) = call(&app, "PUT", "/api/v1/models/roles", Some(API), Some(change.clone())).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "the API token cannot change a role: {body}");
            // Nothing changed.
            let (_, table) = call(&app, "GET", "/api/v1/models/roles", Some(UI), None).await;
            assert_eq!(model_of(&table, "chat").1, "follows_agent");
            // The UI token can.
            let (status, table) = call(&app, "PUT", "/api/v1/models/roles", Some(UI), Some(change)).await;
            assert_eq!(status, StatusCode::OK, "{table}");
            assert_eq!(model_of(&table, "chat"), ("Llama-3.2-3B-Q4_K_M.gguf".to_string(), "set_by_you".to_string()));
            assert_eq!(model_of(&table, "agent").0, "Qwen3-8B-Q4_K_M.gguf", "the other roles stay where they were");
            let warnings = table["warnings"].as_array().unwrap();
            assert_eq!(warnings.len(), 1, "two roles on two models: {table}");
            assert!(warnings[0].as_str().unwrap().contains("chat uses Llama-3.2-3B-Q4_K_M.gguf"));
        });
    }

    #[test]
    fn the_table_says_which_speed_experiment_switches_are_on_and_with_none_set_nothing_is() {
        let (_world, app) = world_with(&["Qwen3-8B-Q4_K_M.gguf"]);
        let off = [("AQUA_LLAMA_CACHE_REUSE", None), ("AQUA_LLAMA_CACHE_RAM", None), ("AQUA_LLAMA_SLOT_SAVE", None), ("AQUA_LLAMA_DRAFT", None), ("AQUA_PLANNER_TOOL_MODE", None), ("AQUA_FAST_TIER", None)];
        crate::flags::with_overrides(&off, || {
            run(async {
                for token in [API, UI] {
                    let (status, table) = call(&app, "GET", "/api/v1/models/roles", Some(token), None).await;
                    assert_eq!(status, StatusCode::OK);
                    let report = &table["speed_experiments"];
                    assert_eq!(report["on"], json!([]), "{report}");
                    assert_eq!(report["warnings"], json!([]));
                    assert!(report["switches"].as_array().unwrap().iter().all(|s| s["state"] == "off"), "{report}");
                }
            });
        });
        // One on, one with a value that is not on the list: the report says so, and quotes nothing that was typed.
        let some = [("AQUA_LLAMA_CACHE_REUSE", Some("128")), ("AQUA_LLAMA_CACHE_RAM", Some("secret-looking-value")), ("AQUA_LLAMA_SLOT_SAVE", None), ("AQUA_LLAMA_DRAFT", None)];
        crate::flags::with_overrides(&some, || {
            run(async {
                let (_, table) = call(&app, "GET", "/api/v1/models/roles", Some(UI), None).await;
                let report = &table["speed_experiments"];
                let state = |name: &str| report["switches"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap()["state"].clone();
                assert_eq!(state("AQUA_LLAMA_CACHE_REUSE"), "on");
                assert_eq!(state("AQUA_LLAMA_CACHE_RAM"), "refused");
                assert!(!table.to_string().contains("secret-looking-value"), "{table}");
            });
        });
    }

    #[test]
    fn a_choice_that_cannot_be_made_is_refused_with_the_reason_and_nothing_changes() {
        let (world, app) = world_with(&["Qwen3-8B-Q4_K_M.gguf"]);
        run(async {
            for (body, word) in [
                (json!({"roles": {"agent": "Missing.gguf"}}), "no model called Missing.gguf"),
                (json!({"roles": {"nobody": "Qwen3-8B-Q4_K_M.gguf"}}), "is not a role"),
                (json!({"roles": {"agent": "openai/gpt-4o"}}), "remote policy"),
                (json!({"roles": {"agent": "../x.gguf"}}), "not a provider on this machine"),
                (json!({"roles": {"agent": "notes.txt"}}), "not the name of a .gguf file"),
                (json!({"roles": {"chat": "Qwen3-8B-Q4_K_M.gguf", "fast": "Missing.gguf"}}), "no model called"),
                (json!({"roless": {}}), "roless"),
            ] {
                let (status, answer) = call(&app, "PUT", "/api/v1/models/roles", Some(UI), Some(body.clone())).await;
                assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY, "{body}: {status} {answer}");
                let said = answer["error"].as_str().map(str::to_string).unwrap_or_else(|| answer.to_string());
                if status == StatusCode::BAD_REQUEST {
                    assert!(said.contains(word), "{body}: {said}");
                }
            }
            let (_, table) = call(&app, "GET", "/api/v1/models/roles", Some(UI), None).await;
            assert_eq!(model_of(&table, "chat").1, "follows_agent", "the one good change of a request that had a bad one was not made either");
            assert!(!world.path("models/router.json").exists(), "nothing was written");
        });
    }

    #[test]
    fn a_choice_is_kept_in_the_file_and_taken_away_with_null() {
        let (world, app) = world_with(&["Qwen3-8B-Q4_K_M.gguf", "Llama-3.2-3B-Q4_K_M.gguf"]);
        run(async {
            let (status, _) = call(&app, "PUT", "/api/v1/models/roles", Some(UI), Some(json!({"roles": {"agent": "llama.cpp/llama-3.2-3b-q4_k_m.gguf", "fast": "Qwen3-8B-Q4_K_M.gguf"}}))).await;
            assert_eq!(status, StatusCode::OK);
            let saved = std::fs::read_to_string(world.path("models/router.json")).unwrap();
            assert!(saved.contains("\"agent\": \"llama.cpp/Llama-3.2-3B-Q4_K_M.gguf\"") && saved.contains("\"fast\": \"llama.cpp/Qwen3-8B-Q4_K_M.gguf\""), "{saved}");
            let (_, table) = call(&app, "GET", "/api/v1/models/roles", Some(UI), None).await;
            assert_eq!(model_of(&table, "agent"), ("Llama-3.2-3B-Q4_K_M.gguf".to_string(), "set_by_you".to_string()));
            assert_eq!(model_of(&table, "decision"), ("Llama-3.2-3B-Q4_K_M.gguf".to_string(), "follows_agent".to_string()));
            // Null and an empty text both take the choice away.
            let (status, table) = call(&app, "PUT", "/api/v1/models/roles", Some(UI), Some(json!({"roles": {"agent": null, "fast": ""}}))).await;
            assert_eq!(status, StatusCode::OK, "{table}");
            assert_eq!(model_of(&table, "agent"), ("Qwen3-8B-Q4_K_M.gguf".to_string(), "default_rule".to_string()));
            assert_eq!(model_of(&table, "fast").1, "follows_agent");
            assert!(!std::fs::read_to_string(world.path("models/router.json")).unwrap().contains("Llama"), "the file says what the table says");
        });
    }

    #[test]
    fn the_use_button_beats_the_qwen3_preference_and_the_status_says_so() {
        // The regression of D-PR-2: a Qwen3-8B file in the folder, the person presses Use on another model, and the agent used to go on with the Qwen3.
        let (_world, app) = world_with(&["Qwen3-8B-Q4_K_M.gguf", "Llama-3.2-3B-Q4_K_M.gguf"]);
        run(async {
            let (_, before) = call(&app, "GET", "/api/v1/status", Some(API), None).await;
            let roles = before["llm_roles"].as_array().expect("the status lists the roles");
            assert_eq!(roles.iter().find(|r| r["role"] == "agent").unwrap()["model"], "Qwen3-8B-Q4_K_M.gguf");
            let (status, answer) = call(&app, "POST", "/api/v1/llama/models", Some(API), Some(json!({"model": "Llama-3.2-3B-Q4_K_M.gguf"}))).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
            let (status, answer) = call(&app, "POST", "/api/v1/llama/models", Some(UI), Some(json!({"model": "Llama-3.2-3B-Q4_K_M.gguf"}))).await;
            assert_eq!(status, StatusCode::OK, "{answer}");
            assert_eq!(answer["active_filename"], "Llama-3.2-3B-Q4_K_M.gguf");
            let (_, after) = call(&app, "GET", "/api/v1/status", Some(API), None).await;
            let find = |role: &str| after["llm_roles"].as_array().unwrap().iter().find(|r| r["role"] == role).unwrap().clone();
            assert_eq!((find("agent")["model"].clone(), find("agent")["source"].clone()), (json!("Llama-3.2-3B-Q4_K_M.gguf"), json!("set_by_you")), "{after}");
            assert_eq!(find("chat")["model"], "Llama-3.2-3B-Q4_K_M.gguf");
            assert_eq!(find("decision")["model"], "Llama-3.2-3B-Q4_K_M.gguf", "what nobody chose follows the agent");
            // A model that is not there changes nothing and says so.
            let (status, answer) = call(&app, "POST", "/api/v1/llama/models", Some(UI), Some(json!({"model": "Nothing.gguf"}))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
            let (_, again) = call(&app, "GET", "/api/v1/models/roles", Some(UI), None).await;
            assert_eq!(model_of(&again, "agent").0, "Llama-3.2-3B-Q4_K_M.gguf");
        });
    }

    #[test]
    fn a_model_deleted_after_it_was_chosen_puts_its_roles_back_on_the_default() {
        let (world, app) = world_with(&["Qwen3-8B-Q4_K_M.gguf", "Llama-3.2-3B-Q4_K_M.gguf"]);
        run(async {
            let (status, _) = call(&app, "POST", "/api/v1/llama/models", Some(UI), Some(json!({"model": "Llama-3.2-3B-Q4_K_M.gguf"}))).await;
            assert_eq!(status, StatusCode::OK);
            std::fs::remove_file(world.path("models/llm/Llama-3.2-3B-Q4_K_M.gguf")).unwrap();
            let (_, table) = call(&app, "GET", "/api/v1/models/roles", Some(UI), None).await;
            let agent = table["roles"].as_array().unwrap().iter().find(|r| r["role"] == "agent").unwrap().clone();
            assert_eq!(agent["model"], "Qwen3-8B-Q4_K_M.gguf");
            assert_eq!(agent["source"], "default_rule");
            assert!(agent["note"].as_str().unwrap().contains("no longer in the model folder"), "{agent}");
        });
    }

    fn serving() -> crate::test_support::model_server::FakeModelServer {
        use crate::test_support::model_server::{completion, respond, respond_json, FakeModelServer};
        FakeModelServer::start(|req, s, _| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v1/models") => respond_json(s, 200, &json!({ "data": [{ "id": "qwen3:8b" }] })),
            ("POST", "/v1/chat/completions") => respond_json(s, 200, &completion("OK")),
            _ => respond(s, 404, &[], "not found"),
        })
    }

    #[test]
    fn the_model_providers_are_off_until_switched_on_listed_with_either_token_and_tried_with_the_ui_token_only() {
        let (world, app) = world_with(&[]);
        let server = serving();
        world.write("models/providers.json", format!(r#"{{ "providers": {{ "ollama": {{ "base_url": "{}", "models": {{ "qwen3:8b": {{}} }} }}, "far": {{ "base_url": "https://api.openai.com/v1" }} }} }}"#, server.base_url()));
        // Off (the default): the file is not read, and the page says which switch.
        run(async {
            for token in [API, UI] {
                let (status, off) = call(&app, "GET", "/api/v1/models/providers", Some(token), None).await;
                assert_eq!(status, StatusCode::OK, "{off}");
                assert_eq!((off["enabled"].clone(), off["file"]["state"].clone(), off["switch"].clone()), (json!(false), json!("off"), json!("AQUA_MODEL_PROVIDERS")));
            }
            assert_eq!(call(&app, "GET", "/api/v1/models/providers", None, None).await.0, StatusCode::UNAUTHORIZED);
            let (status, refused) = call(&app, "POST", "/api/v1/models/providers/test", Some(UI), Some(json!({ "provider": "ollama" }))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(refused["error"].as_str().unwrap().contains("AQUA_MODEL_PROVIDERS=on"), "{refused}");
        });
        // On.
        crate::flags::with_overrides(&[("AQUA_MODEL_PROVIDERS", Some("on"))], || {
            run(async {
                for token in [API, UI] {
                    let (status, on) = call(&app, "GET", "/api/v1/models/providers", Some(token), None).await;
                    assert_eq!(status, StatusCode::OK, "{on}");
                    assert_eq!((on["enabled"].clone(), on["file"]["state"].clone()), (json!(true), json!("ok")));
                    assert_eq!(on["providers"][0]["provider"], "ollama");
                    assert_eq!(on["providers"][0]["models"][0]["model"], "ollama/qwen3:8b");
                    assert_eq!(on["refused"][0]["provider"], "far", "an address of another machine is listed with its reason: {on}");
                    assert!(on["refused"][0]["reason"].as_str().unwrap().contains("not on this machine"));
                }
                let body = json!({ "provider": "ollama", "model": "qwen3:8b" });
                assert_eq!(call(&app, "POST", "/api/v1/models/providers/test", None, Some(body.clone())).await.0, StatusCode::UNAUTHORIZED);
                let (status, denied) = call(&app, "POST", "/api/v1/models/providers/test", Some(API), Some(body.clone())).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "the API token cannot open a connection to a model server: {denied}");
                assert_eq!(server.requests().len(), 0, "and nothing was asked");
                let (status, report) = call(&app, "POST", "/api/v1/models/providers/test", Some(UI), Some(body)).await;
                assert_eq!(status, StatusCode::OK, "{report}");
                assert_eq!((report["reachable"].clone(), report["chat"]["ok"].clone(), report["chat"]["text"].clone()), (json!(true), json!(true), json!("OK")));
                assert_eq!(report["server_models"], json!(["qwen3:8b"]));
                // The gate is in the listing now.
                let (_, listing) = call(&app, "GET", "/api/v1/models/providers", Some(API), None).await;
                assert_eq!(listing["gate"]["allowed"], 1);
                // What cannot be tried is a 400 with the reason: another machine, a model the file does not list, a provider that is not there, a field that is not known.
                for (body, word) in [
                    (json!({ "provider": "far" }), "not on this machine"),
                    (json!({ "provider": "ollama", "model": "gemma3:4b" }), "not listed under ollama"),
                    (json!({ "provider": "nobody" }), "no provider is called nobody"),
                ] {
                    let (status, answer) = call(&app, "POST", "/api/v1/models/providers/test", Some(UI), Some(body.clone())).await;
                    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {answer}");
                    assert!(answer["error"].as_str().unwrap().contains(word), "{body}: {answer}");
                }
                let (status, _) = call(&app, "POST", "/api/v1/models/providers/test", Some(UI), Some(json!({ "provider": "ollama", "prompt": "send this instead" }))).await;
                assert!(status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST, "the words of the call are not the caller's: {status}");
                assert_eq!(server.requests().iter().filter(|r| r.method == "POST").count(), 1, "only the one test call was made");
            });
        });
    }

    #[test]
    fn a_model_deleted_through_the_route_is_forgotten_by_the_roles_at_once() {
        let (world, app) = world_with(&["Qwen3-8B-Q4_K_M.gguf", "Llama-3.2-3B-Q4_K_M.gguf"]);
        run(async {
            let (status, _) = call(&app, "POST", "/api/v1/llama/models", Some(UI), Some(json!({"model": "Llama-3.2-3B-Q4_K_M.gguf"}))).await;
            assert_eq!(status, StatusCode::OK);
            assert!(std::fs::read_to_string(world.path("models/router.json")).unwrap().contains("Llama"));
            let (status, answer) = call(&app, "DELETE", "/api/v1/llama/models", Some(UI), Some(json!({"model": "Llama-3.2-3B-Q4_K_M.gguf"}))).await;
            assert_eq!(status, StatusCode::OK, "{answer}");
            assert!(!std::fs::read_to_string(world.path("models/router.json")).unwrap().contains("Llama"), "the file no longer names the deleted model");
            let (_, table) = call(&app, "GET", "/api/v1/models/roles", Some(UI), None).await;
            let agent = table["roles"].as_array().unwrap().iter().find(|r| r["role"] == "agent").unwrap().clone();
            assert_eq!((agent["model"].clone(), agent["source"].clone()), (json!("Qwen3-8B-Q4_K_M.gguf"), json!("default_rule")));
            assert!(agent.get("note").is_none(), "nothing to say about a choice that is gone: {agent}");
        });
    }
}
