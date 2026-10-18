//! Production FalkorDB bridge for the installed desktop app.
//!
//! The old bridge lived inside Vite's development server, which is absent from an installed
//! application. These routes live on Aqua Core instead, use the normal authenticated gateway,
//! and keep the password in the operating system credential store rather than in the config file.

use super::server::AppState;
use aqua_native_tools::db::redis::{Conn, Reply, Target};
use aqua_native_tools::secrets::{CredentialManager, Secrets};
use axum::{http::StatusCode, response::{IntoResponse, Response}, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::{BTreeMap, BTreeSet}, path::PathBuf, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

const PASSWORD_SECRET: &str = "company/falkordb";
const MAX_QUERY_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FalkorConfig {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    username: String,
    #[serde(default = "default_graph")]
    graph_name: String,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    allow_insecure_remote: bool,
}

fn default_host() -> String { "127.0.0.1".into() }
fn default_port() -> u16 { 6379 }
fn default_graph() -> String { "AquaCompanyBrain".into() }

impl Default for FalkorConfig {
    fn default() -> Self {
        Self { host: default_host(), port: default_port(), username: String::new(), graph_name: default_graph(), tls: false, allow_insecure_remote: false }
    }
}

impl FalkorConfig {
    fn validate(&self) -> Result<(), String> {
        if self.host.is_empty() || self.host.len() > 253 || !self.host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')) {
            return Err("host must be a plain host name or IP address".into());
        }
        if self.port == 0 || matches!(self.port, 8767 | 1420) {
            return Err("the selected port is not allowed".into());
        }
        if self.graph_name.is_empty() || self.graph_name.len() > 80 || !self.graph_name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')) {
            return Err("graphName must contain only letters, digits, _, -, . or :".into());
        }
        if self.username.len() > 128 || self.username.chars().any(char::is_control) {
            return Err("username is invalid".into());
        }
        if !is_local(&self.host) && !self.tls && !self.allow_insecure_remote {
            return Err("remote FalkorDB connections require TLS; explicitly enable insecure remote transport only for a trusted endpoint".into());
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigUpdate {
    host: String,
    port: u16,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    graph_name: String,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    allow_insecure_remote: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryRequest {
    cypher: String,
    #[serde(default)]
    params: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompanyAskRequest {
    question: String,
    #[serde(default = "default_tenant")]
    tenant_id: String,
    #[serde(default = "default_session")]
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompanyActionRequest {
    kind: String,
    ticket_id: String,
    assignee_id: String,
    #[serde(default = "default_tenant")]
    tenant_id: String,
    #[serde(default = "default_session")]
    session_id: String,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompanySessionRequest {
    #[serde(default = "default_tenant")]
    tenant_id: String,
    #[serde(default = "default_session")]
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompanyHandoffRequest {
    summary: String,
    #[serde(default = "default_tenant")]
    tenant_id: String,
    #[serde(default = "default_session")]
    session_id: String,
}

fn default_tenant() -> String { "demo".into() }
fn default_session() -> String { "session_demo_primary".into() }

fn is_local(host: &str) -> bool {
    matches!(host.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

fn config_path() -> PathBuf {
    crate::runtime::data_dir().join("falkordb.json")
}

fn load_config() -> Result<FalkorConfig, String> {
    if let Some(host) = std::env::var("FALKORDB_HOST").ok().filter(|value| !value.trim().is_empty()) {
        let config = FalkorConfig {
            host: host.trim().into(),
            port: std::env::var("FALKORDB_PORT").ok().and_then(|value| value.parse().ok()).unwrap_or_else(default_port),
            username: std::env::var("FALKORDB_USER").unwrap_or_default(),
            graph_name: std::env::var("FALKORDB_GRAPH").unwrap_or_else(|_| default_graph()),
            tls: std::env::var("FALKORDB_TLS").is_ok_and(|value| value.eq_ignore_ascii_case("true")),
            allow_insecure_remote: std::env::var("FALKORDB_ALLOW_INSECURE_REMOTE").is_ok_and(|value| value.eq_ignore_ascii_case("true")),
        };
        config.validate()?;
        return Ok(config);
    }
    let path = config_path();
    if !path.is_file() { return Ok(FalkorConfig::default()); }
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let config: FalkorConfig = serde_json::from_slice(&bytes).map_err(|e| format!("{} is invalid: {e}", path.display()))?;
    config.validate()?;
    Ok(config)
}

fn save_config(config: &FalkorConfig) -> Result<(), String> {
    config.validate()?;
    let path = config_path();
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?).map_err(|e| format!("could not write FalkorDB config: {e}"))?;
    std::fs::rename(&temp, &path).map_err(|e| format!("could not replace FalkorDB config: {e}"))
}

fn connection(config: &FalkorConfig) -> Result<Conn, String> {
    config.validate()?;
    let password = std::env::var("FALKORDB_PASSWORD").ok().filter(|value| !value.is_empty())
        .map(|value| Ok(Some(value))).unwrap_or_else(|| CredentialManager.get(PASSWORD_SECRET).map_err(|e| format!("Credential Manager: {e}")))?;
    connection_with_password(config, password)
}

fn connection_with_password(config: &FalkorConfig, password: Option<String>) -> Result<Conn, String> {
    config.validate()?;
    let target = Target {
        host: config.host.clone(),
        port: config.port,
        user: (!config.username.is_empty()).then(|| config.username.clone()),
        password,
        database: 0,
        tls: config.tls,
    };
    Conn::connect(&target, Duration::from_secs(15)).map_err(|e| e.to_string())
}

fn literal(value: &Value) -> Result<String, String> {
    match value {
        Value::Null => Ok("null".into()),
        Value::Bool(v) => Ok(v.to_string()),
        Value::Number(v) => Ok(v.to_string()),
        Value::String(v) if v.len() <= 32_000 && !v.contains('\0') => Ok(format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'").replace('\r', "\\r").replace('\n', "\\n"))),
        Value::Array(values) if values.len() <= 100 => Ok(format!("[{}]", values.iter().map(literal).collect::<Result<Vec<_>, _>>()?.join(","))),
        _ => Err("query parameters must be bounded scalar JSON values".into()),
    }
}

fn with_params(cypher: &str, params: &BTreeMap<String, Value>) -> Result<String, String> {
    if cypher.is_empty() || cypher.len() > MAX_QUERY_BYTES || cypher.contains('\0') { return Err("cypher must be between 1 byte and 64 KiB".into()); }
    let mut assignments = Vec::with_capacity(params.len());
    for (name, value) in params {
        if name.is_empty() || name.len() > 80 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { return Err("a query parameter name is invalid".into()); }
        assignments.push(format!("{name}={}", literal(value)?));
    }
    Ok(if assignments.is_empty() { cypher.to_string() } else { format!("CYPHER {} {cypher}", assignments.join(" ")) })
}

fn appears_read_only(cypher: &str) -> bool {
    let upper = cypher.to_ascii_uppercase();
    ![" CREATE ", " MERGE ", " DELETE ", " DETACH ", " SET ", " REMOVE ", " DROP "].iter().any(|word| format!(" {upper} ").contains(word))
}

fn run_query(config: &FalkorConfig, cypher: &str, params: &BTreeMap<String, Value>) -> Result<Vec<BTreeMap<String, String>>, String> {
    let query = with_params(cypher, params)?;
    let mut conn = connection(config)?;
    let command = if appears_read_only(cypher) { "GRAPH.RO_QUERY" } else { "GRAPH.QUERY" };
    let reply = conn.send(&[command.into(), config.graph_name.clone(), query, "--compact".into()]).map_err(|e| e.to_string())?;
    if let Reply::Error(message) = reply { return Err(format!("FalkorDB query failed: {message}")); }
    crate::company_brain::parse_rows(&reply).map_err(|e| e.to_string())
}

fn number(rows: &[BTreeMap<String, String>], field: &str) -> u64 {
    rows.first().and_then(|row| row.get(field)).and_then(|value| value.parse().ok()).unwrap_or(0)
}

fn problem(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(json!({ "error": error.into() }))).into_response()
}

async fn status() -> Response {
    let started = Instant::now();
    let result = tokio::task::spawn_blocking(|| {
        let config = load_config()?;
        let nodes = run_query(&config, "MATCH (n) RETURN count(n) AS node_count", &BTreeMap::new())?;
        let edges = run_query(&config, "MATCH ()-[r]->() RETURN count(r) AS edge_count", &BTreeMap::new())?;
        Ok::<_, String>((config, number(&nodes, "node_count"), number(&edges, "edge_count")))
    }).await;
    let latency = started.elapsed().as_millis() as u64;
    match result {
        Ok(Ok((config, nodes, edges))) => Json(json!({
            "connected": true, "mode": "falkordb_core", "host": config.host, "port": config.port,
            "username": config.username, "graphName": config.graph_name, "nodeCount": nodes,
            "edgeCount": edges, "latencyMs": latency
        })).into_response(),
        Ok(Err(error)) => {
            let config = load_config().unwrap_or_default();
            Json(json!({ "connected": false, "mode": "offline", "host": config.host, "port": config.port,
                "username": config.username, "graphName": config.graph_name, "nodeCount": 0,
                "edgeCount": 0, "latencyMs": latency, "error": error })).into_response()
        }
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "the FalkorDB health check stopped unexpectedly"),
    }
}

async fn query(Json(body): Json<QueryRequest>) -> Response {
    match tokio::task::spawn_blocking(move || run_query(&load_config()?, &body.cypher, &body.params)).await {
        Ok(Ok(data)) => Json(json!({ "success": true, "data": data, "metadata": [], "statistics": {} })).into_response(),
        Ok(Err(error)) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "success": false, "data": [], "error": error }))).into_response(),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "the FalkorDB query stopped unexpectedly"),
    }
}

async fn snapshot() -> Response {
    let result = tokio::task::spawn_blocking(|| {
        let config = load_config()?;
        let nodes = run_query(&config, "MATCH (n) RETURN coalesce(n.id, toString(id(n))) AS id, labels(n)[0] AS label, coalesce(n.name, '') AS name, coalesce(n.title, '') AS title, coalesce(n.summary, '') AS summary, coalesce(n.prompt, '') AS prompt LIMIT 150", &BTreeMap::new())?;
        let rels = run_query(&config, "MATCH (a)-[r]->(b) RETURN coalesce(a.id, toString(id(a))) AS source, coalesce(b.id, toString(id(b))) AS target, type(r) AS label LIMIT 300", &BTreeMap::new())?;
        let nodes: Vec<Value> = nodes.into_iter().map(|row| json!({ "id": row.get("id").cloned().unwrap_or_default(), "labels": [row.get("label").cloned().unwrap_or_default()], "properties": { "name": row.get("name").cloned().unwrap_or_default(), "title": row.get("title").cloned().unwrap_or_default(), "summary": row.get("summary").cloned().unwrap_or_default(), "prompt": row.get("prompt").cloned().unwrap_or_default() } })).collect();
        let edges: Vec<Value> = rels.into_iter().map(|row| json!({ "source": row.get("source").cloned().unwrap_or_default(), "target": row.get("target").cloned().unwrap_or_default(), "label": row.get("label").cloned().unwrap_or_default() })).collect();
        Ok::<_, String>((nodes, edges))
    }).await;
    match result {
        Ok(Ok((nodes, edges))) => Json(json!({ "nodes": nodes, "edges": edges })).into_response(),
        Ok(Err(error)) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "nodes": [], "edges": [], "error": error }))).into_response(),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "the graph snapshot stopped unexpectedly"),
    }
}

async fn configure(Json(body): Json<ConfigUpdate>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let config = FalkorConfig { host: body.host.trim().into(), port: body.port, username: body.username.trim().into(), graph_name: body.graph_name.trim().into(), tls: body.tls, allow_insecure_remote: body.allow_insecure_remote };
        config.validate()?;
        let password = if body.password.is_empty() {
            CredentialManager.get(PASSWORD_SECRET).map_err(|e| format!("Credential Manager: {e}"))?
        } else {
            Some(body.password.clone())
        };
        // Test before replacing either the saved settings or the saved credential.
        let mut conn = connection_with_password(&config, password)?;
        match conn.send(&["PING".into()]).map_err(|e| e.to_string())? {
            Reply::Error(error) => return Err(error),
            _ => {}
        }
        if !body.password.is_empty() { CredentialManager.set(PASSWORD_SECRET, &body.password).map_err(|e| format!("Credential Manager: {e}"))?; }
        save_config(&config)?;
        Ok(config)
    }).await;
    match result {
        Ok(Ok(config)) => Json(json!({ "success": true, "config": { "host": config.host, "port": config.port, "username": config.username, "graphName": config.graph_name, "tls": config.tls, "allowInsecureRemote": config.allow_insecure_remote } })).into_response(),
        Ok(Err(error)) => problem(StatusCode::BAD_GATEWAY, error),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "saving the FalkorDB configuration stopped unexpectedly"),
    }
}

async fn init_schema() -> Response {
    let statements = [
        "CREATE INDEX FOR (t:Task) ON (t.id)", "CREATE INDEX FOR (e:Experience) ON (e.id)",
        "CREATE INDEX FOR (tool:Tool) ON (tool.name)", "CREATE INDEX FOR (s:Skill) ON (s.name)",
    ];
    let result = tokio::task::spawn_blocking(move || {
        let config = load_config()?;
        let mut logs = Vec::new();
        for statement in statements { match run_query(&config, statement, &BTreeMap::new()) { Ok(_) => logs.push(format!("Executed: {statement}")), Err(error) => logs.push(format!("Notice: {statement} ({error})")) } }
        Ok::<_, String>(logs)
    }).await;
    match result { Ok(Ok(logs)) => Json(json!({ "success": true, "logs": logs })).into_response(), Ok(Err(error)) => problem(StatusCode::SERVICE_UNAVAILABLE, error), Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "schema initialization stopped unexpectedly") }
}

async fn seed() -> Response {
    const CYPHER: &str = "MERGE (u:User {id:'usr_owner'}) SET u.name='Primary Operator',u.role='Administrator' MERGE (t:Task {id:'tsk_01'}) SET t.prompt='Audit system permissions and security',t.category='security',t.status='verified' MERGE (e:Experience {id:'exp_01'}) SET e.summary='Verify tool execution policy before run',e.solution='Check ToolsPanel policy; prompt if locked',e.confidence=0.98,e.outcome='success' MERGE (tool:Tool {name:'tools.audit_log'}) SET tool.permission_level='standard',tool.domain='security' MERGE (skill:Skill {name:'Daily Security Checkup'}) SET skill.description='Performs comprehensive audit of system rules' MERGE (u)-[:REQUESTS]->(t) MERGE (t)-[:RECALLS]->(e) MERGE (e)-[:USES]->(tool) MERGE (e)-[:FORGED_INTO]->(skill) MERGE (skill)-[:DEPENDS_ON]->(tool) RETURN e.id AS experience_id";
    match tokio::task::spawn_blocking(|| run_query(&load_config()?, CYPHER, &BTreeMap::new())).await {
        Ok(Ok(_)) => Json(json!({ "success": true, "message": "FalkorDB agent memory graph seeded through Aqua Core." })).into_response(),
        Ok(Err(error)) => problem(StatusCode::SERVICE_UNAVAILABLE, error),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "seeding FalkorDB stopped unexpectedly"),
    }
}

fn plain_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 80 && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

fn company_scope(tenant: &str, session: &str) -> Result<(), String> {
    if !plain_id(tenant) { return Err("tenantId must be a plain identifier".into()); }
    if !plain_id(session) { return Err("sessionId must be a plain identifier".into()); }
    Ok(())
}

fn now_id(prefix: &str) -> String {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    format!("{prefix}_{millis}")
}

fn search_needle(question: &str) -> String {
    const STOP: &[&str] = &["about", "affects", "and", "answer", "earlier", "for", "from", "handle", "incident", "project", "should", "source", "sources", "support", "that", "the", "this", "what", "which", "who", "why", "with"];
    question
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .find(|word| word.len() >= 3 && !STOP.contains(word))
        .unwrap_or("")
        .to_string()
}

fn row_value(row: &BTreeMap<String, String>, key: &str) -> String {
    row.get(key).cloned().unwrap_or_default()
}

fn compose_company_answer(question: &str, rows: &[BTreeMap<String, String>]) -> Option<Value> {
    let first = rows.first()?;
    let lower = question.to_ascii_lowercase();
    let intent = if ["who", "owner", "owns", "assign", "handle", "responsible"].iter().any(|word| lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| part == *word)) {
        "ownership"
    } else if ["why", "decision", "decided", "rationale", "reason"].iter().any(|word| lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| part == *word)) {
        "decision"
    } else {
        "project_context"
    };
    let ticket_id = row_value(first, "ticket_id");
    let ticket_title = row_value(first, "ticket_title");
    let project_id = row_value(first, "project_id");
    let project_name = row_value(first, "project_name");
    let owner_id = row_value(first, "owner_id");
    let owner_name = row_value(first, "owner_name");
    let owner_role = row_value(first, "owner_role");
    let team_name = row_value(first, "team_name");
    let decision_id = row_value(first, "decision_id");
    let decision_title = row_value(first, "decision_title");
    let decision_summary = row_value(first, "decision_summary");
    let owner_sentence = format!("{owner_name}, {owner_role} on {team_name}, owns {project_name} and should handle {ticket_id}.");
    let decision_sentence = format!("{decision_id} ({decision_title}) affects that project: {decision_summary}");
    let answer = match intent {
        "ownership" => format!("{owner_sentence} {decision_sentence}"),
        "decision" => format!("{decision_sentence} The accountable owner is {owner_name}."),
        _ => format!("{ticket_title} belongs to {project_name}. {owner_sentence} {decision_sentence}"),
    };
    let mut source_ids = BTreeSet::new();
    let mut sources = Vec::new();
    for row in rows {
        let id = row_value(row, "source_id");
        if id.is_empty() || !source_ids.insert(id.clone()) { continue; }
        sources.push(json!({
            "id": id, "title": row_value(row, "source_title"), "kind": row_value(row, "source_kind"),
            "uri": row_value(row, "source_uri"),
            "updatedAt": row.get("source_updated_at").and_then(|value| value.parse::<u64>().ok())
        }));
    }
    let mut paths = vec![json!([ticket_id, "BELONGS_TO", project_name, "OWNED_BY", owner_name]), json!([decision_id, "AFFECTS", project_name])];
    for source in &sources {
        paths.push(json!([decision_id, "SUPPORTED_BY", source.get("title").and_then(Value::as_str).unwrap_or_default()]));
    }
    let action = if ticket_id.is_empty() || owner_id.is_empty() { Value::Null } else { json!({
        "kind": "assign_ticket", "label": format!("Assign {ticket_id} to {owner_name}"), "ticketId": ticket_id, "assigneeId": owner_id
    }) };
    Some(json!({
        "answer": answer, "intent": intent, "action": action, "paths": paths, "sources": sources,
        "facts": { "ticketId": ticket_id, "ticketTitle": ticket_title, "projectId": project_id,
            "projectName": project_name, "ownerId": owner_id, "ownerName": owner_name, "ownerRole": owner_role,
            "teamName": team_name, "decisionId": decision_id, "decisionTitle": decision_title, "decisionSummary": decision_summary }
    }))
}

const COMPANY_CONTEXT_QUERY: &str = "MATCH (ticket:Ticket)-[:BELONGS_TO]->(project:Project) MATCH (owner:Person)-[:OWNS]->(project) MATCH (owner)-[:MEMBER_OF]->(team:Team) MATCH (decision:Decision)-[:AFFECTS]->(project) MATCH (decision)-[:SUPPORTED_BY]->(source:Source) WHERE ticket.tenant_id=$tenantId AND coalesce(ticket.active,true) AND coalesce(project.active,true) AND coalesce(owner.active,true) AND coalesce(decision.active,true) AND coalesce(source.active,true) AND ($needle='' OR toLower(ticket.title) CONTAINS $needle OR toLower(project.name) CONTAINS $needle OR toLower(decision.title) CONTAINS $needle) RETURN ticket.id AS ticket_id,ticket.title AS ticket_title,project.id AS project_id,project.name AS project_name,owner.id AS owner_id,owner.name AS owner_name,owner.role AS owner_role,team.name AS team_name,decision.id AS decision_id,decision.title AS decision_title,decision.summary AS decision_summary,source.id AS source_id,source.title AS source_title,source.kind AS source_kind,source.uri AS source_uri,source.updated_at AS source_updated_at ORDER BY source.updated_at DESC LIMIT 10";

async fn company_seed() -> Response {
    const CYPHER: &str = "MERGE (company:Company {id:'company_acme',tenant_id:'demo'}) SET company.name='Acme Cloud',company.active=true MERGE (team:Team {id:'team_identity',tenant_id:'demo'}) SET team.name='Identity Engineering',team.active=true MERGE (person:Person {id:'person_priya',tenant_id:'demo'}) SET person.name='Priya Raman',person.role='Staff Engineer',person.active=true MERGE (project:Project {id:'project_identity',tenant_id:'demo'}) SET project.name='Identity Platform',project.status='active',project.active=true MERGE (system:System {id:'system_oauth_gateway',tenant_id:'demo'}) SET system.name='OAuth Gateway',system.status='active',system.active=true MERGE (ticket:Ticket {id:'INC-1042',tenant_id:'demo'}) SET ticket.title='OAuth production incident',ticket.status='open',ticket.priority='P1',ticket.active=true MERGE (decision:Decision {id:'DEC-014',tenant_id:'demo'}) SET decision.title='Use staged PKCE migration',decision.summary='Roll out PKCE by client cohort and preserve the legacy callback during migration.',decision.decided_at=1760486400000,decision.active=true MERGE (adr:Source:Document {id:'SRC-ADR-014',tenant_id:'demo'}) SET adr.title='ADR-014: OAuth PKCE migration',adr.kind='document',adr.uri='company://docs/ADR-014',adr.updated_at=1760486400000,adr.active=true MERGE (chat:Source:Chat {id:'SRC-CHAT-221',tenant_id:'demo'}) SET chat.title='Identity incident handoff',chat.kind='chat',chat.uri='company://chat/identity/221',chat.updated_at=1760572800000,chat.active=true MERGE (person)-[:MEMBER_OF]->(team) MERGE (team)-[:PART_OF]->(company) MERGE (person)-[:OWNS]->(project) MERGE (ticket)-[:BELONGS_TO]->(project) MERGE (project)-[:DEPENDS_ON]->(system) MERGE (decision)-[:AFFECTS]->(project) MERGE (decision)-[:DECIDED_BY]->(person) MERGE (decision)-[:SUPPORTED_BY]->(adr) MERGE (decision)-[:SUPPORTED_BY]->(chat) MERGE (ticket)-[:SUPPORTED_BY]->(chat) RETURN company.id AS company_id,project.id AS project_id,ticket.id AS ticket_id,decision.id AS decision_id";
    match tokio::task::spawn_blocking(|| run_query(&load_config()?, CYPHER, &BTreeMap::new())).await {
        Ok(Ok(data)) => Json(json!({ "success": true, "message": "Company Brain dataset is ready in FalkorDB.", "data": data, "statistics": {} })).into_response(),
        Ok(Err(error)) => problem(StatusCode::SERVICE_UNAVAILABLE, error),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "preparing Company Brain stopped unexpectedly"),
    }
}

async fn company_ask(Json(body): Json<CompanyAskRequest>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        company_scope(&body.tenant_id, &body.session_id)?;
        let question = body.question.trim();
        if question.is_empty() || question.chars().count() > 4_000 { return Err("a question between 1 and 4000 characters is required".to_string()); }
        let config = load_config()?;
        let mut params = BTreeMap::from([("tenantId".into(), json!(body.tenant_id)), ("needle".into(), json!(search_needle(question)))]);
        let mut rows = run_query(&config, COMPANY_CONTEXT_QUERY, &params)?;
        if rows.is_empty() { params.insert("needle".into(), json!("")); rows = run_query(&config, COMPANY_CONTEXT_QUERY, &params)?; }
        let Some(mut grounded) = compose_company_answer(question, &rows) else { return Ok::<_, String>(None); };
        let interaction_id = now_id("question");
        let answer_id = now_id("answer");
        let source_ids: Vec<Value> = grounded["sources"].as_array().into_iter().flatten().filter_map(|source| source.get("id").cloned()).collect();
        let persist = "MERGE (agent:Agent {id:'agent_aqua',tenant_id:$tenantId}) SET agent.name='Aqua Company Brain',agent.role='company_brain' MERGE (user:User {id:'user_demo',tenant_id:$tenantId}) SET user.name='Demo Operator' MERGE (session:Session {id:$sessionId,tenant_id:$tenantId}) ON CREATE SET session.created_at=timestamp() CREATE (q:Question {id:$interactionId,tenant_id:$tenantId,text:$question,intent:$intent,answered_at:timestamp()}) CREATE (a:Answer {id:$answerId,tenant_id:$tenantId,text:$answer,created_at:timestamp()}) MERGE (agent)-[:PARTICIPATED_IN]->(session) MERGE (user)-[:OWNS_SESSION]->(session) CREATE (session)-[:CONTAINS]->(q) CREATE (q)-[:ANSWERED_BY]->(a) CREATE (agent)-[:PRODUCED]->(a) WITH q,a MATCH (s:Source) WHERE s.tenant_id=$tenantId AND s.id IN $sourceIds MERGE (q)-[:ANSWERED_WITH]->(s) MERGE (a)-[:GROUNDED_IN]->(s) RETURN q.id AS interaction_id";
        let persist_params = BTreeMap::from([
            ("tenantId".into(), json!(body.tenant_id)), ("sessionId".into(), json!(body.session_id)),
            ("interactionId".into(), json!(interaction_id)), ("answerId".into(), json!(answer_id)),
            ("question".into(), json!(question)), ("intent".into(), grounded["intent"].clone()),
            ("answer".into(), grounded["answer"].clone()), ("sourceIds".into(), Value::Array(source_ids)),
        ]);
        run_query(&config, persist, &persist_params)?;
        let object = grounded.as_object_mut().ok_or_else(|| "could not compose the company answer".to_string())?;
        object.insert("success".into(), json!(true));
        object.insert("interactionId".into(), json!(interaction_id));
        object.insert("answerId".into(), json!(answer_id));
        object.insert("sessionId".into(), json!(body.session_id));
        object.insert("graphRequired".into(), json!(true));
        Ok::<_, String>(Some(grounded))
    }).await;
    match result {
        Ok(Ok(Some(answer))) => Json(answer).into_response(),
        Ok(Ok(None)) => problem(StatusCode::NOT_FOUND, "No connected company context was found for this question."),
        Ok(Err(error)) => problem(StatusCode::SERVICE_UNAVAILABLE, error),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "the Company Brain query stopped unexpectedly"),
    }
}

async fn company_action(Json(body): Json<CompanyActionRequest>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        company_scope(&body.tenant_id, &body.session_id)?;
        if body.kind != "assign_ticket" || !plain_id(&body.ticket_id) || !plain_id(&body.assignee_id) { return Err("a valid assign_ticket action is required".into()); }
        let action_id = now_id("action");
        let outcome_id = now_id("outcome");
        let query = "MATCH (ticket:Ticket {id:$ticketId,tenant_id:$tenantId})-[:BELONGS_TO]->(project:Project) MATCH (person:Person {id:$assigneeId,tenant_id:$tenantId})-[:OWNS]->(project) MERGE (ticket)-[assignment:ASSIGNED_TO]->(person) SET assignment.assigned_at=timestamp(),assignment.assigned_by='company_brain_agent' CREATE (action:Action {id:$actionId,kind:'assign_ticket',tenant_id:$tenantId,status:'completed',created_at:timestamp()}) CREATE (action)-[:ACTED_ON]->(ticket) CREATE (action)-[:SELECTED_FROM_GRAPH]->(person) WITH action,ticket,person MERGE (agent:Agent {id:'agent_aqua',tenant_id:$tenantId}) SET agent.name='Aqua Company Brain',agent.role='company_brain' MERGE (session:Session {id:$sessionId,tenant_id:$tenantId}) MERGE (session)-[:CONTAINS]->(action) MERGE (agent)-[:PERFORMED]->(action) CREATE (outcome:Outcome {id:$outcomeId,tenant_id:$tenantId,status:'success',summary:'Ticket assignment validated and completed',recorded_at:timestamp()}) CREATE (action)-[:RESULTED_IN]->(outcome) RETURN action.id AS action_id,ticket.id AS ticket_id,person.name AS assignee";
        let params = BTreeMap::from([("ticketId".into(),json!(body.ticket_id)),("assigneeId".into(),json!(body.assignee_id)),("tenantId".into(),json!(body.tenant_id)),("actionId".into(),json!(action_id)),("outcomeId".into(),json!(outcome_id)),("sessionId".into(),json!(body.session_id))]);
        let rows = run_query(&load_config()?, query, &params)?;
        Ok::<_, String>((action_id, body.ticket_id, rows))
    }).await;
    match result {
        Ok(Ok((_, _, rows))) if rows.is_empty() => problem(StatusCode::CONFLICT, "The graph did not validate this assignment."),
        Ok(Ok((action_id, ticket_id, _))) => Json(json!({"success":true,"message":format!("{ticket_id} was assigned using the ownership path in FalkorDB."),"actionId":action_id})).into_response(),
        Ok(Err(error)) => problem(StatusCode::SERVICE_UNAVAILABLE, error),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "the Company Brain action stopped unexpectedly"),
    }
}

async fn company_memory(Json(body): Json<CompanySessionRequest>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        company_scope(&body.tenant_id, &body.session_id)?;
        let config = load_config()?;
        let params = BTreeMap::from([("tenantId".into(),json!(body.tenant_id)),("sessionId".into(),json!(body.session_id))]);
        let question_rows = run_query(&config, "MATCH (session:Session {id:$sessionId,tenant_id:$tenantId})-[:CONTAINS]->(q:Question) MATCH (q)-[:ANSWERED_BY]->(a:Answer) OPTIONAL MATCH (a)-[:GROUNDED_IN]->(source:Source) RETURN q.id AS question_id,q.text AS question,a.id AS answer_id,a.text AS answer,source.id AS source_id,q.answered_at AS answered_at ORDER BY answered_at DESC LIMIT 50", &params)?;
        let mut questions: BTreeMap<String, Value> = BTreeMap::new();
        for row in question_rows {
            let id = row_value(&row,"question_id");
            let entry = questions.entry(id.clone()).or_insert_with(|| json!({"questionId":id,"question":row_value(&row,"question"),"answerId":row_value(&row,"answer_id"),"answer":row_value(&row,"answer"),"sourceIds":[]}));
            let source = row_value(&row,"source_id");
            if !source.is_empty() { if let Some(ids)=entry.get_mut("sourceIds").and_then(Value::as_array_mut) { if !ids.iter().any(|id| id.as_str()==Some(&source)) { ids.push(json!(source)); } } }
        }
        let actions = run_query(&config, "MATCH (session:Session {id:$sessionId,tenant_id:$tenantId})-[:CONTAINS]->(action:Action) OPTIONAL MATCH (action)-[:ACTED_ON]->(target) RETURN action.id AS action_id,action.kind AS kind,action.status AS status,target.id AS target_id,action.created_at AS created_at ORDER BY created_at DESC LIMIT 10", &params)?.into_iter().map(|row| json!({"actionId":row_value(&row,"action_id"),"kind":row_value(&row,"kind"),"status":row_value(&row,"status"),"targetId":row_value(&row,"target_id")})).collect::<Vec<_>>();
        let handoffs = run_query(&config, "MATCH (session:Session {id:$sessionId,tenant_id:$tenantId})-[:CONTAINS]->(handoff:Handoff) MATCH (from:Agent)-[:HANDED_OFF]->(handoff)-[:TO_AGENT]->(to:Agent) RETURN handoff.id AS handoff_id,from.name AS from_agent,to.name AS to_agent,handoff.summary AS summary,handoff.created_at AS created_at ORDER BY created_at DESC LIMIT 10", &params)?.into_iter().map(|row| json!({"handoffId":row_value(&row,"handoff_id"),"fromAgent":row_value(&row,"from_agent"),"toAgent":row_value(&row,"to_agent"),"summary":row_value(&row,"summary")})).collect::<Vec<_>>();
        Ok::<_, String>(json!({"success":true,"sessionId":body.session_id,"questions":questions.into_values().collect::<Vec<_>>(),"actions":actions,"handoffs":handoffs}))
    }).await;
    match result { Ok(Ok(memory)) => Json(memory).into_response(), Ok(Err(error)) => problem(StatusCode::SERVICE_UNAVAILABLE,error), Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR,"recalling Company Brain memory stopped unexpectedly") }
}

async fn company_handoff(Json(body): Json<CompanyHandoffRequest>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        company_scope(&body.tenant_id,&body.session_id)?;
        let summary=body.summary.trim();
        if summary.is_empty() || summary.chars().count()>32_000 { return Err("a handoff summary between 1 and 32000 characters is required".into()); }
        let handoff_id=now_id("handoff");
        let query="MERGE (session:Session {id:$sessionId,tenant_id:$tenantId}) MERGE (from:Agent {id:'agent_aqua',tenant_id:$tenantId}) SET from.name='Aqua Company Brain',from.role='company_brain' MERGE (to:Agent {id:'agent_incident',tenant_id:$tenantId}) SET to.name='Incident Response Agent',to.role='incident_response' CREATE (handoff:Handoff {id:$handoffId,tenant_id:$tenantId,summary:$summary,created_at:timestamp()}) MERGE (session)-[:CONTAINS]->(handoff) CREATE (from)-[:HANDED_OFF]->(handoff) CREATE (handoff)-[:TO_AGENT]->(to) RETURN handoff.id AS handoff_id";
        let params=BTreeMap::from([("tenantId".into(),json!(body.tenant_id)),("sessionId".into(),json!(body.session_id)),("handoffId".into(),json!(handoff_id)),("summary".into(),json!(summary))]);
        run_query(&load_config()?,query,&params)?;
        Ok::<_,String>(handoff_id)
    }).await;
    match result { Ok(Ok(handoff_id)) => Json(json!({"success":true,"handoffId":handoff_id,"message":"Handoff persisted for the Incident Response Agent."})).into_response(), Ok(Err(error)) => problem(StatusCode::SERVICE_UNAVAILABLE,error), Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR,"creating the Company Brain handoff stopped unexpectedly") }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/falkordb/status", get(status))
        .route("/api/v1/falkordb/query", post(query))
        .route("/api/v1/falkordb/snapshot", get(snapshot))
        .route("/api/v1/falkordb/config", post(configure))
        .route("/api/v1/falkordb/init", post(init_schema))
        .route("/api/v1/falkordb/seed", post(seed))
        .route("/api/v1/falkordb/company/seed", post(company_seed))
        .route("/api/v1/falkordb/company/ask", post(company_ask))
        .route("/api/v1/falkordb/company/action", post(company_action))
        .route("/api/v1/falkordb/company/memory", post(company_memory))
        .route("/api/v1/falkordb/company/handoff", post(company_handoff))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_plaintext_requires_an_explicit_opt_in() {
        let mut config = FalkorConfig { host: "db.example.test".into(), ..FalkorConfig::default() };
        assert!(config.validate().unwrap_err().contains("require TLS"));
        config.allow_insecure_remote = true;
        assert!(config.validate().is_ok());
        config.allow_insecure_remote = false;
        config.tls = true;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn parameters_are_bounded_and_escaped_before_the_graph_command() {
        let params = BTreeMap::from([("needle".into(), json!("Priya's OAuth\\path"))]);
        let query = with_params("MATCH (n {name:$needle}) RETURN n.id", &params).unwrap();
        assert!(query.starts_with("CYPHER needle='Priya\\'s OAuth\\\\path' MATCH"), "{query}");
        assert!(with_params("RETURN 1", &BTreeMap::from([("bad name".into(), json!(1))])).is_err());
    }

    #[test]
    fn graph_mutations_do_not_use_the_read_only_command() {
        assert!(appears_read_only("MATCH (n) RETURN count(n)"));
        for query in ["CREATE (:Task)", "MATCH (n) SET n.ok=true", "MATCH (n) DELETE n", "MERGE (:User {id:'u'})"] {
            assert!(!appears_read_only(query), "{query}");
        }
    }
}
