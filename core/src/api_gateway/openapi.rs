//! The description of the API as an OpenAPI 3 document (`documentation/openapi.json`, served at `GET /api/v1/openapi.json`).
//!
//! It is **generated from the router's own source** by a test (`AQUA_UPDATE_DOCS=1 cargo test -p aqua-core --lib openapi` rewrites it, a plain run
//! fails when the file is out of date), so a route cannot be added, renamed or removed without the document saying so. For each route it gives the path,
//! the methods, the first sentence of the handler's doc comment, and which token it needs: `none` (health), `api` (the API token) or `ui` (the UI token:
//! the handler calls `ui_scope_required`). The bodies are described by `contract_schemas` (card OA10-02): `components.schemas` holds the schemas, every operation says what its
//! errors look like (`Error`, with the `code` of `documentation/ERROR_CODES.md`), and the routes in `contract_schemas::ROUTE_TYPES` say what their request and answer are
//! (`x-aqua-typed: true`). A route that is not in that table has `x-aqua-typed: false` and a body no one has described yet.

/// The document the build carries.
pub const SPEC: &str = include_str!("../../../documentation/openapi.json");

/// The document as a value, for `GET /api/v1/openapi.json`.
pub fn spec() -> serde_json::Value {
    serde_json::from_str(SPEC).unwrap_or_else(|_| serde_json::json!({ "openapi": "3.0.3", "info": { "title": "Aqua", "version": "0" }, "paths": {} }))
}

#[cfg(test)]
mod generate {
    use super::super::contract_schemas;
    use regex::Regex;
    use serde_json::{json, Map, Value};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    const FILES: [&str; 40] = ["preapproval_api.rs", "contract.rs", "server.rs", "server/status_api.rs", "server/index_api.rs", "server/ws.rs", "server/approvals_api.rs", "server/pipeline_api.rs", "server/stream_api.rs", "server/turn_control_api.rs", "server/listen_api.rs", "server/speech_api.rs", "server/voice_settings_api.rs", "server/llm_api.rs", "server/mcp_api.rs", "server/tools_api.rs", "server/browser_api.rs", "server/agent_api.rs", "server/agent_stream_api.rs", "models_api.rs", "route_api.rs", "flow_api.rs", "flow_sched_api.rs", "flow_comments_api.rs", "workflow_api.rs", "learning_api.rs", "devices_api.rs", "dictation_api.rs", "announce_api.rs", "attachments_api.rs", "voice_choice_api.rs", "recipes_api.rs", "routing_feedback_api.rs", "autonomy_api.rs", "self_repair_api.rs", "eval_api.rs", "run_api.rs", "run_control_api.rs", "sessions_api.rs", "coding_api.rs"];

    fn gateway_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("api_gateway")
    }

    fn doc_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("documentation").join("openapi.json")
    }

    /// The source of a file without its tests (the routes of a test's own small router are not part of the API).
    fn without_tests(text: &str) -> &str {
        // The first test module and everything after it. (A single function marked as test-only above it is not the end of the real code.)
        let marker = Regex::new(r"(?m)^#\[cfg\(test\)\]\s*
\s*(?:pub(?:\([a-z]+\))? )?mod ").expect("a valid pattern");
        match marker.find(text) {
            Some(m) => &text[..m.start()],
            None => text,
        }
    }

    /// The text of the call after `.route(` up to its matching close, or `None`.
    fn balanced(text: &str, open_at: usize) -> Option<&str> {
        let bytes = text.as_bytes();
        let (mut depth, mut i, mut in_str) = (0i32, open_at, false);
        while i < bytes.len() {
            let c = bytes[i];
            if in_str {
                if c == b'\\' {
                    i += 1;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(&text[open_at + 1..i]);
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        None
    }

    struct Handler {
        summary: String,
        auth: &'static str,
    }

    /// The doc comment's first sentence and the token the handler needs, from the function's source.
    fn handler(text: &str, name: &str) -> Handler {
        let name = name.rsplit("::").next().unwrap_or(name);
        let re = Regex::new(&format!(r"(?m)^(?:pub(?:\([a-z]+\))? )?(?:async )?fn {}\b", regex::escape(name))).expect("a valid pattern");
        let Some(m) = re.find(text) else { return Handler { summary: String::new(), auth: "api" } };
        // The doc comment is the `///` lines right above.
        let mut docs: Vec<&str> = text[..m.start()].lines().rev().take_while(|l| l.trim_start().starts_with("///") || l.trim_start().starts_with("#[")).filter(|l| l.trim_start().starts_with("///")).collect();
        docs.reverse();
        let joined = docs.iter().map(|l| l.trim_start().trim_start_matches("///").trim()).collect::<Vec<_>>().join(" ");
        let first = joined.split_inclusive(". ").next().unwrap_or("").trim().trim_end_matches('.').to_string();
        // The body: up to the next top-level function.
        let rest = &text[m.end()..];
        let end = Regex::new(r"(?m)^(?:pub(?:\([a-z]+\))? )?(?:async )?fn ").expect("a valid pattern").find(rest).map(|e| e.start()).unwrap_or(rest.len());
        let auth = if rest[..end].contains("ui_scope_required") { "ui" } else { "api" };
        Handler { summary: first, auth }
    }

    /// Every route of the gateway: (path, method) -> (summary, auth).
    pub fn routes() -> BTreeMap<(String, String), (String, &'static str)> {
        let route_re = Regex::new(r"\.route\(").expect("a valid pattern");
        let method_re = Regex::new(r"(?:^|[^A-Za-z_])(get|post|put|delete|patch)\(\s*([A-Za-z_][A-Za-z0-9_:]*)").expect("a valid pattern");
        let mut out = BTreeMap::new();
        for file in FILES {
            let raw = std::fs::read_to_string(gateway_dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
            let text = without_tests(&raw);
            for m in route_re.find_iter(text) {
                let Some(call) = balanced(text, m.end() - 1) else { continue };
                let Some((path, rest)) = call.trim().strip_prefix('"').and_then(|s| s.split_once('"')) else { continue };
                let rest = rest.trim_start().trim_start_matches(',');
                for c in method_re.captures_iter(rest) {
                    let (method, name) = (c[1].to_string(), c[2].to_string());
                    let h = handler(text, &name);
                    let auth = if method == "get" && path == "/api/v1/health" { "none" } else { h.auth };
                    out.insert((path.to_string(), method), (h.summary, auth));
                }
            }
        }
        out
    }

    fn operation_id(path: &str, method: &str) -> String {
        let words: Vec<String> = path
            .trim_start_matches("/api/v1")
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_matches(|c| c == ':' || c == '{' || c == '}').split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w[..1].to_uppercase() + &w[1..]).collect::<String>())
            .collect();
        format!("{method}{}", words.concat())
    }

    fn contract_schemas_ref(name: &str) -> Value {
        json!({ "$ref": format!("#/components/schemas/{name}") })
    }

    /// The body of a request: the schema of its type, or `Untyped` for a route that is not typed (a `GET` and a route that takes no body have none).
    fn request_body(method: &str, typed: Option<&contract_schemas::RouteType>, all: &Map<String, Value>) -> Option<Value> {
        if !matches!(method, "post" | "put" | "patch") {
            return None;
        }
        match typed.map(|t| t.request) {
            Some(contract_schemas::Body::Empty) => None,
            Some(contract_schemas::Body::Of(name)) => {
                let always = all.get(name).and_then(|s| s.get("required")).and_then(Value::as_array).is_some_and(|r| !r.is_empty());
                Some(json!({ "required": always, "content": { "application/json": { "schema": contract_schemas_ref(name) } } }))
            }
            None => Some(json!({ "required": false, "content": { "application/json": { "schema": contract_schemas_ref("Untyped") } } })),
        }
    }

    /// What a route answers: its answers when it is typed, and, for every route, what an error looks like (card OA10-02).
    fn responses(typed: Option<&contract_schemas::RouteType>) -> Value {
        let error = |name: &str, about: &str| json!({ "description": about, "content": { "application/json": { "schema": contract_schemas_ref(name) } } });
        let mut out = Map::new();
        match typed {
            Some(t) if t.stream.is_some() => {
                out.insert("200".into(), json!({ "description": "A stream of server-sent events: the `data:` of each is JSON of this schema (`x-aqua-events` names the schema of each kind of event, when they differ)", "content": { "text/event-stream": { "schema": contract_schemas_ref(t.stream.unwrap_or("Untyped")) } } }));
            }
            Some(t) => {
                for (status, name) in t.ok {
                    let about = match status {
                        200 => "OK",
                        202 => "Accepted: it goes on after the answer",
                        422 => "Refused by a rule: the body is the same shape as an answer",
                        _ => "An answer",
                    };
                    out.insert(status.to_string(), json!({ "description": about, "content": { "application/json": { "schema": contract_schemas_ref(name) } } }));
                }
            }
            None => {
                out.insert("200".into(), json!({ "description": "OK (the body is not described yet)" }));
            }
        }
        let errors = typed.map_or("Error", |t| t.errors);
        if errors == "Error" {
            out.insert("400".into(), error("Error", "The request is wrong; the body says why"));
            out.insert("403".into(), error("Error", "This needs the UI token"));
        } else {
            // The gate's refusals are `Error` bodies; what the handler answers on failure is this route's own envelope.
            out.insert("403".into(), error("Error", "This needs the UI token"));
        }
        out.insert("401".into(), error("Error", "No or a wrong token"));
        out.insert("default".into(), error(errors, "An error: `code` is one of documentation/ERROR_CODES.md"));
        Value::Object(out)
    }

    pub fn render() -> String {
        let all_schemas = contract_schemas::schemas();
        let mut paths: Map<String, Value> = Map::new();
        for ((path, method), (summary, auth)) in routes() {
            // `:id` in the router is `{id}` in OpenAPI.
            let oa_path = Regex::new(r":([A-Za-z_][A-Za-z0-9_]*)").expect("a valid pattern").replace_all(&path, "{$1}").into_owned();
            let typed = contract_schemas::route_type(&method, &oa_path);
            let mut params: Vec<Value> = Regex::new(r"\{([A-Za-z_][A-Za-z0-9_]*)\}")
                .expect("a valid pattern")
                .captures_iter(&oa_path)
                .map(|c| json!({ "name": &c[1], "in": "path", "required": true, "schema": { "type": if typed.is_some_and(|t| t.ints.contains(&&c[1])) { "integer" } else { "string" } } }))
                .collect();
            if let Some(t) = typed {
                params.extend(t.query.iter().map(|(name, kind)| json!({ "name": name, "in": "query", "required": false, "schema": { "type": kind } })));
            }
            let mut op = Map::new();
            op.insert("operationId".into(), json!(operation_id(&oa_path, &method)));
            op.insert("summary".into(), json!(if summary.is_empty() { format!("{} {}", method.to_uppercase(), oa_path) } else { summary }));
            op.insert("x-aqua-token".into(), json!(auth));
            if !params.is_empty() {
                op.insert("parameters".into(), Value::Array(params));
            }
            op.insert("x-aqua-typed".into(), json!(typed.is_some()));
            if let Some(body) = request_body(&method, typed, &all_schemas) {
                op.insert("requestBody".into(), body);
            }
            op.insert("responses".into(), responses(typed));
            if let Some(t) = typed.filter(|t| !t.events.is_empty()) {
                op.insert("x-aqua-events".into(), Value::Object(t.events.iter().map(|(name, schema)| (name.to_string(), contract_schemas_ref(schema))).collect()));
            }
            if auth == "none" {
                op.insert("security".into(), json!([]));
            }
            paths.entry(oa_path).or_insert_with(|| json!({})).as_object_mut().expect("an object").insert(method, Value::Object(op));
        }
        let doc = json!({
            "openapi": "3.0.3",
            "info": {
                "title": "Aqua local API",
                "version": env!("CARGO_PKG_VERSION"),
                "description": "Generated from the router's source and from core/src/api_gateway/contract_schemas.rs by a test; do not edit by hand. Aqua listens on 127.0.0.1 only. `x-aqua-token` says which token a route needs: `none`, `api` (the API token) or `ui` (the UI token: it can approve actions and read what the person asked). `x-aqua-typed` says whether the request and the answer of a route are described by a schema (`false`: the body is not described yet). Every error body is `Error`: its `code` is one of documentation/ERROR_CODES.md. Every answer carries the header `x-aqua-contract`, the number in `x-aqua-contract` below.",
                "x-aqua-contract": super::super::contract::CONTRACT_VERSION,
            },
            "servers": [{ "url": "http://127.0.0.1:8767" }],
            "components": { "securitySchemes": { "bearer": { "type": "http", "scheme": "bearer" } }, "schemas": Value::Object(all_schemas) },
            "security": [{ "bearer": [] }],
            "paths": Value::Object(paths),
        });
        let mut text = serde_json::to_string_pretty(&doc).expect("a document serialises");
        text.push('\n');
        text
    }

    fn update_requested() -> bool {
        matches!(std::env::var("AQUA_UPDATE_DOCS").ok().as_deref(), Some("1" | "true"))
    }

    #[test]
    fn the_openapi_document_is_current() {
        let fresh = render();
        let path = doc_path();
        if update_requested() {
            std::fs::write(&path, &fresh).expect("the document is written");
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
        assert!(have == fresh, "documentation/openapi.json is out of date: run AQUA_UPDATE_DOCS=1 cargo test -p aqua-core --lib openapi");
    }

    #[test]
    fn every_route_of_the_gateway_is_in_the_document_with_the_token_it_needs() {
        let all = routes();
        assert!(all.len() > 120, "only {} routes found: the scan is wrong", all.len());
        let auth = |p: &str, m: &str| all.get(&(p.to_string(), m.to_string())).unwrap_or_else(|| panic!("{m} {p} is not in the scan")).1;
        assert_eq!(auth("/api/v1/health", "get"), "none");
        assert_eq!(auth("/api/v1/pipeline/text", "post"), "api");
        assert_eq!(auth("/api/v1/runs", "get"), "ui", "a run record holds what the person asked");
        assert_eq!(auth("/api/v1/runs/:run_id/resume", "post"), "ui");
        assert_eq!(auth("/api/v1/consents", "get"), "ui");
        assert_eq!(auth("/api/v1/consents/:id", "delete"), "ui");
        assert_eq!(auth("/api/v1/queue/:job_id/cancel", "post"), "ui");
        assert_eq!(auth("/api/v1/dictation/settings", "put"), "ui");
        assert_eq!(auth("/api/v1/mcp/agent", "post"), "api");
        // The three routes that drive the person's browser and screen directly, with no run behind them, are the app's: a script has the agent routes.
        for direct in ["/api/v1/browser/call", "/api/v1/browser-mcp/call", "/api/v1/windows-mcp/call"] {
            assert_eq!(auth(direct, "post"), "ui", "{direct}");
        }
        // The routes of a test's own small router are not the API.
        assert!(!all.keys().any(|(p, _)| p.contains("/runs/1/stream")), "a test route was counted");
        // The document the build carries parses, lists the same routes and says what a route does.
        let spec = super::spec();
        let paths = spec["paths"].as_object().expect("paths");
        assert_eq!(paths.values().map(|v| v.as_object().map_or(0, |o| o.len())).sum::<usize>(), all.len(), "the carried document and the source disagree");
        assert!(paths["/api/v1/runs/{run_id}/undo"]["post"]["summary"].as_str().unwrap().starts_with("Puts back what one finished run changed"), "{}", paths["/api/v1/runs/{run_id}/undo"]["post"]["summary"]);
        assert_eq!(paths["/api/v1/health"]["get"]["security"], json!([]), "health needs no token");
        assert_eq!(paths["/api/v1/runs/{run_id}/undo"]["post"]["parameters"][0]["name"], "run_id");
    }

    /// Every `$ref` in `value`, with where it is.
    fn refs_in(value: &Value, at: &str, out: &mut Vec<(String, String)>) {
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    if key == "$ref" {
                        out.push((at.to_string(), v.as_str().unwrap_or("").to_string()));
                    } else {
                        refs_in(v, &format!("{at}/{key}"), out);
                    }
                }
            }
            Value::Array(items) => items.iter().enumerate().for_each(|(n, v)| refs_in(v, &format!("{at}/{n}"), out)),
            _ => {}
        }
    }

    #[test]
    fn every_listed_route_has_schemas() {
        let spec = super::spec();
        let all = routes();
        let mut typed_seen = 0;
        for (path, operations) in spec["paths"].as_object().expect("paths") {
            for (method, op) in operations.as_object().expect("operations") {
                let at = format!("{} {path}", method.to_uppercase());
                // Every operation says what its errors look like, and the body of a refusal of the gate is the same for all of them.
                assert_eq!(op["responses"]["401"]["content"]["application/json"]["schema"]["$ref"], "#/components/schemas/Error", "{at}: a 401 is an Error");
                assert!(op["responses"]["default"]["content"]["application/json"]["schema"]["$ref"].is_string(), "{at}: no schema for an error");
                assert!(op["responses"]["200"].is_object() || op["responses"]["202"].is_object(), "{at}: no answer");
                // A route that takes a body says which: a schema, or `Untyped` for one nobody has described. A route typed as taking none has no body.
                let typed = op["x-aqua-typed"].as_bool().unwrap_or_else(|| panic!("{at}: x-aqua-typed is missing"));
                let entry = contract_schemas::route_type(method, path);
                assert_eq!(typed, entry.is_some(), "{at}: x-aqua-typed disagrees with the table");
                typed_seen += usize::from(typed);
                let takes_body = matches!(method.as_str(), "post" | "put" | "patch");
                let body = &op["requestBody"]["content"]["application/json"]["schema"]["$ref"];
                match entry {
                    Some(t) if t.request == contract_schemas::Body::Empty => assert!(op.get("requestBody").is_none(), "{at}: typed as taking no body"),
                    Some(_) => assert!(body.is_string() && body != "#/components/schemas/Untyped", "{at}: typed, with no schema for its body"),
                    None if takes_body => assert_eq!(body, "#/components/schemas/Untyped", "{at}"),
                    None => assert!(op.get("requestBody").is_none(), "{at}: a {method} has no body"),
                }
                if let Some(t) = entry {
                    for (status, name) in t.ok {
                        assert_eq!(op["responses"][status.to_string()]["content"]["application/json"]["schema"]["$ref"], format!("#/components/schemas/{name}"), "{at}: {status}");
                    }
                    if let Some(name) = t.stream {
                        assert_eq!(op["responses"]["200"]["content"]["text/event-stream"]["schema"]["$ref"], format!("#/components/schemas/{name}"), "{at}: a stream");
                    }
                    for (name, _) in t.query {
                        assert!(op["parameters"].as_array().unwrap().iter().any(|p| p["name"] == *name && p["in"] == "query"), "{at}: query {name}");
                    }
                }
            }
        }
        assert_eq!(typed_seen, contract_schemas::ROUTE_TYPES.len(), "a typed route is missing from the document");
        // The table names only routes that the router has, with the method it says.
        let braces = Regex::new(r"\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("a valid pattern");
        for t in contract_schemas::ROUTE_TYPES {
            let colon = braces.replace_all(t.path, ":$1").into_owned();
            assert!(all.contains_key(&(colon, t.method.to_string())), "ROUTE_TYPES has {} {}, which the router does not have", t.method, t.path);
        }
    }

    #[test]
    fn schemas_resolve() {
        let spec = super::spec();
        let known = spec["components"]["schemas"].as_object().expect("the document has components.schemas");
        assert!(known.len() > 50, "{} schemas", known.len());
        let mut found = Vec::new();
        refs_in(&spec, "", &mut found);
        assert!(found.len() > 1000, "only {} references: the scan is wrong", found.len());
        for (at, reference) in &found {
            let name = reference.strip_prefix("#/components/schemas/").unwrap_or_else(|| panic!("{at}: a reference that is not to a schema: {reference}"));
            assert!(known.contains_key(name), "{at}: {name} is not in components.schemas");
        }
        // The version route is in the document, with its schema.
        assert_eq!(spec["paths"]["/api/v1/version"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]["$ref"], "#/components/schemas/Version");
        assert_eq!(spec["info"]["x-aqua-contract"], super::super::contract::CONTRACT_VERSION);
    }

    #[test]
    fn the_codes_of_the_error_schema_are_the_codes_the_code_has() {
        let spec = super::spec();
        let words: Vec<&str> = spec["components"]["schemas"]["ErrorCode"]["enum"].as_array().expect("the codes").iter().filter_map(Value::as_str).collect();
        assert_eq!(words, super::super::contract::ErrorCode::ALL.map(|c| c.as_str()));
    }
}
