//! FalkorDB-backed company context tools.
//!
//! The model never supplies Cypher. It can ask one bounded read question or request one assignment;
//! fixed queries enforce the tenant and the assignment query proves that the assignee owns the
//! ticket's project. Connections use the existing Redis TLS transport and Credential Manager.

use crate::company_ingest::{compile_mutations, ingestion_status, plan_sync, EntityKey, EntityKind, GraphMutation, IngestionEnvelope, IngestionStatus, StoredRecord, SyncStats};
use crate::company_memory::{
    AgentMemory, AnswerMemory, CompanyMemoryBackend, CompanyMemoryError, CompanyMemoryRepository, GraphOperation, GraphResult, GraphRow, GraphStatement, HandoffMemory, QuestionMemory, SessionMemory,
    SharedStateRevision, TenantScope,
};
use crate::company_retrieval::{self, EvidenceClaim, EvidenceRecord, QueryTemplate, RetrievalPlan, RetrievalResult};
use aqua_native_tools::db::redis::{Conn, Reply, Target};
use aqua_native_tools::secrets::{valid_name, Secrets};
use aqua_tool_host::{Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

const SEARCH: &str = "company.search_context";
const EXPLAIN: &str = "company.explain_path";
const PROPOSE: &str = "company.propose_assignment";
const ASSIGN: &str = "company.assign_ticket";
const RECORD_OUTCOME: &str = "company.record_outcome";
const INGEST: &str = "company.ingest";
const OPEN_SESSION: &str = "company.open_session";
const SHARE_STATE: &str = "company.share_state";
const HANDOFF: &str = "company.handoff";
const RECALL_MEMORY: &str = "company.recall_memory";
const REMEMBER_EXCHANGE: &str = "company.remember_exchange";
const MAX_RECORDS: usize = 100;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompanyConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_graph")]
    pub graph: String,
    #[serde(default = "default_tenant")]
    pub tenant_id: String,
    #[serde(default)]
    pub user: Option<String>,
    /// Credential Manager name, never the password itself.
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub tls: bool,
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    6379
}
fn default_graph() -> String {
    "AquaCompanyBrain".into()
}
fn default_tenant() -> String {
    "demo".into()
}

impl Default for CompanyConfig {
    fn default() -> Self {
        Self { enabled: false, host: default_host(), port: default_port(), graph: default_graph(), tenant_id: default_tenant(), user: None, secret: None, tls: false }
    }
}

fn plain_id(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

fn plain_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.chars().count() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace() || matches!(c, '.' | ',' | '-' | '_' | ':' | ';' | '(' | ')' | '/'))
}

fn loopback(host: &str) -> bool {
    matches!(host.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

impl CompanyConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.host.is_empty() || self.host.len() > 253 || !self.host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')) {
            return Err("host must be a plain host name or address".into());
        }
        if self.port == 0 || (loopback(&self.host) && matches!(self.port, 8767 | 8790 | 1420)) {
            return Err(format!("port {} is not allowed", self.port));
        }
        if !plain_id(&self.graph, 80) {
            return Err("graph must be 1-80 letters, digits, _, -, . or :".into());
        }
        if !plain_id(&self.tenant_id, 80) {
            return Err("tenant_id must be 1-80 letters, digits, _, -, . or :".into());
        }
        if let Some(user) = &self.user {
            if !plain_id(user, 80) {
                return Err("user is not valid".into());
            }
        }
        if let Some(secret) = &self.secret {
            if !valid_name(secret) {
                return Err(format!("'{secret}' is not a valid secret name"));
            }
        }
        if !loopback(&self.host) && !self.tls {
            return Err("a FalkorDB connection to another machine must use TLS".into());
        }
        if self.enabled && !loopback(&self.host) && self.secret.is_none() {
            return Err("an enabled remote FalkorDB connection needs a Credential Manager secret name".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextRecord {
    pub ticket_id: String,
    pub ticket_title: String,
    pub project_id: String,
    pub project_name: String,
    pub owner_id: String,
    pub owner_name: String,
    pub owner_role: String,
    pub team_name: String,
    pub decision_id: String,
    pub decision_title: String,
    pub decision_summary: String,
    pub source_id: String,
    pub source_title: String,
    pub source_kind: String,
    pub source_uri: String,
    pub source_updated_at: String,
    /// Alternating entity and relationship identifiers selected by a fixed retrieval template.
    pub path: Vec<String>,
}

struct GraphResponse<T> {
    value: T,
    secrets: Vec<String>,
}

trait CompanyGraph: Send + Sync {
    fn search(&self, template: QueryTemplate) -> Result<GraphResponse<Vec<ContextRecord>>, ToolError>;
    fn assign(&self, ticket_id: &str, assignee_id: &str, action_id: &str) -> Result<GraphResponse<()>, ToolError>;
    fn record_outcome(&self, action_id: &str, outcome_id: &str, status: &str, summary: &str) -> Result<GraphResponse<()>, ToolError>;
    fn ingest(&self, envelope: IngestionEnvelope, stale_after_ms: i64) -> Result<GraphResponse<(SyncStats, IngestionStatus, usize)>, ToolError>;
    fn memory(&self, statement: &GraphStatement) -> Result<GraphResponse<GraphResult>, ToolError>;
}

struct FalkorGraph {
    config: CompanyConfig,
    secrets: Arc<dyn Secrets>,
}

impl FalkorGraph {
    fn connect(&self) -> Result<(Conn, Vec<String>), ToolError> {
        let password = match &self.config.secret {
            Some(name) => self
                .secrets
                .get(name)
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("credential store: {e}")))?
                .ok_or_else(|| ToolError::new(ErrorCode::AuthRequired, format!("the '{name}' credential is not stored; use aqua-cli secret-set {name}")))?,
            None => String::new(),
        };
        let used = (!password.is_empty()).then(|| password.clone()).into_iter().collect();
        let target =
            Target { host: self.config.host.clone(), port: self.config.port, user: self.config.user.clone(), password: (!password.is_empty()).then_some(password), database: 0, tls: self.config.tls };
        Ok((Conn::connect(&target, Duration::from_secs(15))?, used))
    }

    fn send_query(&self, query: String, read_only: bool) -> Result<GraphResponse<Reply>, ToolError> {
        let (mut conn, secrets) = self.connect()?;
        let command = if read_only { "GRAPH.RO_QUERY" } else { "GRAPH.QUERY" };
        let args = vec![command.into(), self.config.graph.clone(), query, "--compact".into()];
        let reply = conn.send(&args)?;
        if let Reply::Error(message) = &reply {
            let low = message.to_ascii_lowercase();
            let code = if low.contains("noauth") {
                ErrorCode::AuthRequired
            } else if low.contains("wrongpass") {
                ErrorCode::AuthFailed
            } else {
                ErrorCode::BackendError
            };
            return Err(ToolError::new(code, format!("FalkorDB query failed: {message}")));
        }
        Ok(GraphResponse { value: reply, secrets })
    }

    fn stored_records(&self, tenant: &str, source_system: &str) -> Result<GraphResponse<Vec<StoredRecord>>, ToolError> {
        if !plain_id(tenant, 80) || !plain_id(source_system, 80) {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "tenant and source system must be plain identifiers"));
        }
        let query = format!(
            "MATCH (n) WHERE n.tenant_id = '{tenant}' AND n.source_system = '{source_system}' AND n.external_id IS NOT NULL RETURN head(labels(n)) AS kind, n.external_id AS external_id, n.content_hash AS content_hash, n.active AS active, n.observed_at AS observed_at, n.stale_at AS stale_at LIMIT 20000"
        );
        let raw = self.send_query(query, true)?;
        let records = parse_rows(&raw.value)?
            .into_iter()
            .filter_map(|row| {
                Some(StoredRecord {
                    key: EntityKey { tenant_id: tenant.into(), source_system: source_system.into(), kind: entity_kind(row.get("kind")?)?, external_id: row.get("external_id")?.clone() },
                    content_hash: row.get("content_hash").cloned().unwrap_or_default(),
                    active: row.get("active").is_none_or(|value| !matches!(value.to_ascii_lowercase().as_str(), "false" | "0")),
                    observed_at_ms: row.get("observed_at").and_then(|value| value.parse().ok()).unwrap_or(0),
                    stale_at_ms: row.get("stale_at").and_then(|value| value.parse().ok()).unwrap_or(0),
                })
            })
            .collect();
        Ok(GraphResponse { value: records, secrets: raw.secrets })
    }

    fn apply_mutation(&self, mutation: &GraphMutation) -> Result<GraphResponse<()>, ToolError> {
        let raw = self.send_query(parameterized_query(&mutation.query, &mutation.parameters)?, false)?;
        if parse_rows(&raw.value)?.is_empty() {
            return Err(ToolError::new(ErrorCode::Conflict, "an ingestion mutation did not return its tenant-scoped record"));
        }
        Ok(GraphResponse { value: (), secrets: raw.secrets })
    }
}

fn entity_kind(label: &str) -> Option<EntityKind> {
    match label {
        "Person" => Some(EntityKind::Person),
        "Team" => Some(EntityKind::Team),
        "Project" => Some(EntityKind::Project),
        "Ticket" => Some(EntityKind::Ticket),
        "Decision" => Some(EntityKind::Decision),
        "Document" => Some(EntityKind::Document),
        "ChatMessage" => Some(EntityKind::ChatMessage),
        "System" => Some(EntityKind::System),
        "Topic" => Some(EntityKind::Topic),
        _ => None,
    }
}

fn cypher_literal(value: &Value) -> Result<String, ToolError> {
    match value {
        Value::Null => Ok("null".into()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::String(value) if !value.contains('\0') && value.len() <= 1_000_000 => {
            let escaped = value.replace('\\', "\\\\").replace('\'', "\\'").replace('\r', "\\r").replace('\n', "\\n");
            Ok(format!("'{escaped}'"))
        }
        Value::Array(values) if values.len() <= 100 => Ok(format!("[{}]", values.iter().map(cypher_literal).collect::<Result<Vec<_>, _>>()?.join(","))),
        _ => Err(ToolError::new(ErrorCode::InvalidArguments, "ingestion parameters must be bounded scalar JSON values")),
    }
}

fn parameterized_query(query: &str, parameters: &BTreeMap<String, Value>) -> Result<String, ToolError> {
    let mut bindings = Vec::with_capacity(parameters.len());
    for (name, value) in parameters {
        if !plain_id(name, 80) {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "a graph parameter name is invalid"));
        }
        bindings.push(format!("{name}={}", cypher_literal(value)?));
    }
    Ok(format!("CYPHER {} {query}", bindings.join(" ")))
}

/// Select one of six audited queries. Keeping this mapping closed prevents the model or caller from
/// injecting Cypher while still making retrieval behavior depend on the classified intent.
fn query_for_template(template: QueryTemplate, tenant: &str) -> String {
    let fresh = "coalesce(source.active, true) AND (source.stale_at IS NULL OR source.stale_at > timestamp())";
    let query = match template {
        QueryTemplate::OwnershipPath => format!(
            "MATCH (ticket:Ticket)-[:BELONGS_TO]->(project:Project)<-[:OWNS]-(owner:Person) OPTIONAL MATCH (owner)-[:MEMBER_OF]->(team:Team) MATCH (ticket)-[:SUPPORTED_BY]->(source:Source) WHERE ticket.tenant_id = '{tenant}' AND project.tenant_id = '{tenant}' AND owner.tenant_id = '{tenant}' AND coalesce(ticket.active, true) AND coalesce(project.active, true) AND coalesce(owner.active, true) AND (ticket.stale_at IS NULL OR ticket.stale_at > timestamp()) AND (project.stale_at IS NULL OR project.stale_at > timestamp()) AND (owner.stale_at IS NULL OR owner.stale_at > timestamp()) AND {fresh} RETURN ticket.id AS ticket_id, ticket.title AS ticket_title, project.id AS project_id, project.name AS project_name, owner.id AS owner_id, owner.name AS owner_name, owner.role AS owner_role, team.name AS team_name, '' AS decision_id, '' AS decision_title, '' AS decision_summary, source.id AS source_id, source.title AS source_title, source.kind AS source_kind, source.uri AS source_uri, source.updated_at AS source_updated_at ORDER BY source.updated_at DESC LIMIT 100"
        ),
        QueryTemplate::DecisionRationale => format!(
            "MATCH (decision:Decision)-[:AFFECTS]->(project:Project) MATCH (decision)-[:SUPPORTED_BY]->(source:Source) OPTIONAL MATCH (ticket:Ticket)-[:BELONGS_TO]->(project) OPTIONAL MATCH (owner:Person)-[:OWNS]->(project) OPTIONAL MATCH (owner)-[:MEMBER_OF]->(team:Team) WHERE decision.tenant_id = '{tenant}' AND project.tenant_id = '{tenant}' AND coalesce(decision.active, true) AND coalesce(project.active, true) AND (decision.stale_at IS NULL OR decision.stale_at > timestamp()) AND (project.stale_at IS NULL OR project.stale_at > timestamp()) AND {fresh} RETURN ticket.id AS ticket_id, ticket.title AS ticket_title, project.id AS project_id, project.name AS project_name, owner.id AS owner_id, owner.name AS owner_name, owner.role AS owner_role, team.name AS team_name, decision.id AS decision_id, decision.title AS decision_title, decision.summary AS decision_summary, source.id AS source_id, source.title AS source_title, source.kind AS source_kind, source.uri AS source_uri, source.updated_at AS source_updated_at ORDER BY source.updated_at DESC LIMIT 100"
        ),
        QueryTemplate::ExpertisePath => format!(
            "MATCH (owner:Person)-[:HAS_EXPERTISE]->(topic:Topic) MATCH (owner)-[:SUPPORTED_BY]->(source:Source) OPTIONAL MATCH (owner)-[:WORKED_ON]->(project:Project) OPTIONAL MATCH (owner)-[:MEMBER_OF]->(team:Team) WHERE owner.tenant_id = '{tenant}' AND topic.tenant_id = '{tenant}' AND coalesce(owner.active, true) AND coalesce(topic.active, true) AND (owner.stale_at IS NULL OR owner.stale_at > timestamp()) AND (topic.stale_at IS NULL OR topic.stale_at > timestamp()) AND {fresh} RETURN topic.id AS ticket_id, topic.title AS ticket_title, project.id AS project_id, project.name AS project_name, owner.id AS owner_id, owner.name AS owner_name, owner.role AS owner_role, team.name AS team_name, '' AS decision_id, '' AS decision_title, '' AS decision_summary, source.id AS source_id, source.title AS source_title, source.kind AS source_kind, source.uri AS source_uri, source.updated_at AS source_updated_at ORDER BY source.updated_at DESC LIMIT 100"
        ),
        QueryTemplate::DependencyPath => format!(
            "MATCH (project:Project)-[dependency:DEPENDS_ON|USES]->(target) MATCH (project)-[:SUPPORTED_BY]->(source:Source) WHERE project.tenant_id = '{tenant}' AND target.tenant_id = '{tenant}' AND coalesce(project.active, true) AND coalesce(target.active, true) AND (project.stale_at IS NULL OR project.stale_at > timestamp()) AND (target.stale_at IS NULL OR target.stale_at > timestamp()) AND {fresh} RETURN '' AS ticket_id, '' AS ticket_title, project.id AS project_id, project.name AS project_name, '' AS owner_id, '' AS owner_name, '' AS owner_role, '' AS team_name, target.id AS decision_id, target.name AS decision_title, type(dependency) AS decision_summary, source.id AS source_id, source.title AS source_title, source.kind AS source_kind, source.uri AS source_uri, source.updated_at AS source_updated_at ORDER BY source.updated_at DESC LIMIT 100"
        ),
        QueryTemplate::IncidentContext => format!(
            "MATCH (ticket:Ticket)-[:BELONGS_TO]->(project:Project)<-[:OWNS]-(owner:Person) MATCH (ticket)-[:SUPPORTED_BY]->(source:Source) OPTIONAL MATCH (owner)-[:MEMBER_OF]->(team:Team) OPTIONAL MATCH (decision:Decision)-[:AFFECTS]->(project) WHERE ticket.tenant_id = '{tenant}' AND project.tenant_id = '{tenant}' AND owner.tenant_id = '{tenant}' AND coalesce(ticket.active, true) AND coalesce(project.active, true) AND coalesce(owner.active, true) AND (ticket.stale_at IS NULL OR ticket.stale_at > timestamp()) AND (project.stale_at IS NULL OR project.stale_at > timestamp()) AND (owner.stale_at IS NULL OR owner.stale_at > timestamp()) AND {fresh} RETURN ticket.id AS ticket_id, ticket.title AS ticket_title, project.id AS project_id, project.name AS project_name, owner.id AS owner_id, owner.name AS owner_name, owner.role AS owner_role, team.name AS team_name, decision.id AS decision_id, decision.title AS decision_title, decision.summary AS decision_summary, source.id AS source_id, source.title AS source_title, source.kind AS source_kind, source.uri AS source_uri, source.updated_at AS source_updated_at ORDER BY source.updated_at DESC LIMIT 100"
        ),
        QueryTemplate::ConnectedContext => format!(
            "MATCH (subject)-[connection]->(connected) MATCH (subject)-[:SUPPORTED_BY]->(source:Source) WHERE subject.tenant_id = '{tenant}' AND connected.tenant_id = '{tenant}' AND type(connection) <> 'SUPPORTED_BY' AND coalesce(subject.active, true) AND coalesce(connected.active, true) AND (subject.stale_at IS NULL OR subject.stale_at > timestamp()) AND (connected.stale_at IS NULL OR connected.stale_at > timestamp()) AND {fresh} RETURN subject.id AS ticket_id, subject.title AS ticket_title, connected.id AS project_id, connected.name AS project_name, '' AS owner_id, '' AS owner_name, '' AS owner_role, '' AS team_name, type(connection) AS decision_id, connected.title AS decision_title, '' AS decision_summary, source.id AS source_id, source.title AS source_title, source.kind AS source_kind, source.uri AS source_uri, source.updated_at AS source_updated_at ORDER BY source.updated_at DESC LIMIT 100"
        ),
    };
    query
}

impl CompanyGraph for FalkorGraph {
    fn search(&self, template: QueryTemplate) -> Result<GraphResponse<Vec<ContextRecord>>, ToolError> {
        let query = query_for_template(template, &self.config.tenant_id);
        let raw = self.send_query(query, true)?;
        let mut rows = parse_context_reply(&raw.value)?;
        for row in &mut rows {
            row.path = context_path(template, row);
        }
        Ok(GraphResponse { value: rows, secrets: raw.secrets })
    }

    fn assign(&self, ticket_id: &str, assignee_id: &str, action_id: &str) -> Result<GraphResponse<()>, ToolError> {
        if !plain_id(ticket_id, 100) || !plain_id(assignee_id, 100) || !plain_id(action_id, 100) {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "ticket_id, assignee_id and action id must be plain identifiers"));
        }
        let query = format!(
            "MATCH (ticket:Ticket {{id: '{ticket_id}', tenant_id: '{}'}})-[:BELONGS_TO]->(project:Project) MATCH (person:Person {{id: '{assignee_id}', tenant_id: '{}'}})-[:OWNS]->(project) MERGE (ticket)-[assignment:ASSIGNED_TO]->(person) SET assignment.assigned_at = timestamp(), assignment.assigned_by = 'aqua_core_company_agent' CREATE (action:Action {{id: '{action_id}', kind: 'assign_ticket', tenant_id: '{}', status: 'completed', created_at: timestamp()}}) CREATE (action)-[:ACTED_ON]->(ticket) CREATE (action)-[:SELECTED_FROM_GRAPH]->(person) RETURN action.id AS action_id",
            self.config.tenant_id, self.config.tenant_id, self.config.tenant_id
        );
        let raw = self.send_query(query, false)?;
        let rows = parse_rows(&raw.value)?;
        if rows.is_empty() {
            return Err(ToolError::new(ErrorCode::Conflict, "FalkorDB did not confirm that this person owns the ticket's project; nothing was assigned"));
        }
        Ok(GraphResponse { value: (), secrets: raw.secrets })
    }

    fn record_outcome(&self, action_id: &str, outcome_id: &str, status: &str, summary: &str) -> Result<GraphResponse<()>, ToolError> {
        if !plain_id(action_id, 100) || !plain_id(outcome_id, 100) || !matches!(status, "succeeded" | "failed") || !plain_text(summary, 240) {
            return Err(ToolError::new(
                ErrorCode::InvalidArguments,
                "action_id and outcome_id must be plain identifiers, status must be succeeded or failed, and summary must be safe plain text up to 240 characters",
            ));
        }
        let query = format!(
            "MATCH (action:Action {{id: '{action_id}', tenant_id: '{}'}}) CREATE (outcome:Outcome {{id: '{outcome_id}', tenant_id: '{}', status: '{status}', summary: '{summary}', recorded_at: timestamp()}}) CREATE (action)-[:RESULTED_IN]->(outcome) RETURN outcome.id AS outcome_id",
            self.config.tenant_id, self.config.tenant_id
        );
        let raw = self.send_query(query, false)?;
        if parse_rows(&raw.value)?.is_empty() {
            return Err(ToolError::new(ErrorCode::NotFound, "FalkorDB did not find this action in the configured tenant; no outcome was recorded"));
        }
        Ok(GraphResponse { value: (), secrets: raw.secrets })
    }

    fn ingest(&self, envelope: IngestionEnvelope, stale_after_ms: i64) -> Result<GraphResponse<(SyncStats, IngestionStatus, usize)>, ToolError> {
        if envelope.tenant_id != self.config.tenant_id {
            return Err(ToolError::new(ErrorCode::PermissionDenied, "the ingestion envelope tenant does not match the configured Company Brain tenant"));
        }
        if envelope.records.len() > 1_000 {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "one ToolHost ingestion call may contain at most 1000 records"));
        }
        let tenant = envelope.tenant_id.clone();
        let source_system = envelope.source_system.clone();
        let observed_at_ms = envelope.observed_at_ms;
        let before = self.stored_records(&tenant, &source_system)?;
        let plan = plan_sync(envelope, &before.value, stale_after_ms).map_err(|error| ToolError::new(ErrorCode::InvalidArguments, error))?;
        let mutations = compile_mutations(&plan);
        if mutations.len() > 10_000 {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "the ingestion plan exceeds 10000 bounded graph mutations"));
        }
        let mut secrets = before.secrets;
        for mutation in &mutations {
            secrets.extend(self.apply_mutation(mutation)?.secrets);
        }
        let after = self.stored_records(&tenant, &source_system)?;
        secrets.extend(after.secrets);
        secrets.sort();
        secrets.dedup();
        let status = ingestion_status(&after.value, observed_at_ms, 0);
        Ok(GraphResponse { value: (plan.stats, status, mutations.len()), secrets })
    }

    fn memory(&self, statement: &GraphStatement) -> Result<GraphResponse<GraphResult>, ToolError> {
        let read_only = statement.operation == GraphOperation::Read;
        let raw = self.send_query(parameterized_query(statement.cypher, &statement.params)?, read_only)?;
        let rows = parse_rows(&raw.value)?.into_iter().map(|row| row.into_iter().map(|(key, value)| (key, Value::String(value))).collect::<GraphRow>()).collect();
        Ok(GraphResponse { value: GraphResult { rows }, secrets: raw.secrets })
    }
}

fn scalar(reply: &Reply) -> Option<String> {
    match reply {
        Reply::Simple(s) => Some(s.clone()),
        Reply::Bulk(Some(bytes)) => std::str::from_utf8(bytes).ok().map(str::to_string),
        Reply::Int(n) => Some(n.to_string()),
        // FalkorDB values are `[type, value]`; descend to the value.
        Reply::Array(Some(items)) if items.len() == 2 && matches!(items.first(), Some(Reply::Int(_))) => scalar(&items[1]),
        _ => None,
    }
}

pub(crate) fn parse_rows(reply: &Reply) -> Result<Vec<BTreeMap<String, String>>, ToolError> {
    let bad = |message: &str| ToolError::new(ErrorCode::BackendError, format!("unexpected FalkorDB result: {message}"));
    let Reply::Array(Some(sections)) = reply else {
        return Err(bad("the result is not an array"));
    };
    if sections.len() < 2 {
        return Err(bad("the result has no header and rows"));
    }
    let Reply::Array(Some(headers)) = &sections[0] else {
        return Err(bad("the header is not an array"));
    };
    let names: Vec<String> = headers
        .iter()
        .filter_map(|h| match h {
            Reply::Array(Some(parts)) => parts.last().and_then(scalar),
            _ => scalar(h),
        })
        .collect();
    let Reply::Array(Some(rows)) = &sections[1] else {
        return Err(bad("the rows are not an array"));
    };
    let mut out = Vec::new();
    for row in rows.iter().take(MAX_RECORDS) {
        let Reply::Array(Some(values)) = row else {
            return Err(bad("a row is not an array"));
        };
        let mapped = names.iter().zip(values).map(|(name, value)| (name.clone(), scalar(value).unwrap_or_default())).collect();
        out.push(mapped);
    }
    Ok(out)
}

fn parse_context_reply(reply: &Reply) -> Result<Vec<ContextRecord>, ToolError> {
    let required = |row: &BTreeMap<String, String>, name: &str| row.get(name).cloned().unwrap_or_default();
    Ok(parse_rows(reply)?
        .into_iter()
        .map(|row| ContextRecord {
            ticket_id: required(&row, "ticket_id"),
            ticket_title: required(&row, "ticket_title"),
            project_id: required(&row, "project_id"),
            project_name: required(&row, "project_name"),
            owner_id: required(&row, "owner_id"),
            owner_name: required(&row, "owner_name"),
            owner_role: required(&row, "owner_role"),
            team_name: required(&row, "team_name"),
            decision_id: required(&row, "decision_id"),
            decision_title: required(&row, "decision_title"),
            decision_summary: required(&row, "decision_summary"),
            source_id: required(&row, "source_id"),
            source_title: required(&row, "source_title"),
            source_kind: required(&row, "source_kind"),
            source_uri: required(&row, "source_uri"),
            source_updated_at: required(&row, "source_updated_at"),
            path: Vec::new(),
        })
        .collect())
}

fn nonempty_path(parts: &[&str]) -> Vec<String> {
    parts.iter().filter(|part| !part.is_empty()).map(|part| (*part).to_string()).collect()
}

fn context_path(template: QueryTemplate, row: &ContextRecord) -> Vec<String> {
    match template {
        QueryTemplate::OwnershipPath => nonempty_path(&[&row.ticket_id, "BELONGS_TO", &row.project_id, "OWNED_BY", &row.owner_id, "SUPPORTED_BY", &row.source_id]),
        QueryTemplate::DecisionRationale => nonempty_path(&[&row.decision_id, "AFFECTS", &row.project_id, "SUPPORTED_BY", &row.source_id]),
        QueryTemplate::ExpertisePath => {
            let mut path = nonempty_path(&[&row.owner_id, "HAS_EXPERTISE", &row.ticket_id]);
            if !row.project_id.is_empty() {
                path.extend(nonempty_path(&["WORKED_ON", &row.project_id]));
            }
            path.extend(nonempty_path(&["SUPPORTED_BY", &row.source_id]));
            path
        }
        QueryTemplate::DependencyPath => nonempty_path(&[&row.project_id, &row.decision_summary, &row.decision_id, "SUPPORTED_BY", &row.source_id]),
        QueryTemplate::IncidentContext => {
            let mut path = nonempty_path(&[&row.ticket_id, "BELONGS_TO", &row.project_id]);
            if !row.owner_id.is_empty() {
                path.extend(nonempty_path(&["OWNED_BY", &row.owner_id]));
            }
            path.extend(nonempty_path(&["SUPPORTED_BY", &row.source_id]));
            path
        }
        QueryTemplate::ConnectedContext => nonempty_path(&[&row.ticket_id, &row.decision_id, &row.project_id, "SUPPORTED_BY", &row.source_id]),
    }
}

fn record_key(row: &ContextRecord) -> String {
    format!("{}:{}", row.path.join("|"), row.source_id)
}

fn terms(question: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "about", "affects", "and", "answer", "earlier", "for", "from", "handle", "incident", "project", "should", "source", "sources", "support", "that", "the", "this", "what", "which", "who", "why",
        "with",
    ];
    question.to_ascii_lowercase().split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_').filter(|word| word.len() >= 3 && !STOP.contains(word)).map(str::to_string).collect()
}

fn relevant(question: &str, rows: Vec<ContextRecord>) -> Vec<ContextRecord> {
    let needles = terms(question);
    if needles.is_empty() {
        return rows.into_iter().take(20).collect();
    }
    let matched: Vec<_> = rows
        .iter()
        .filter(|row| {
            let haystack = format!(
                "{} {} {} {} {} {} {} {} {}",
                row.ticket_id,
                row.ticket_title,
                row.project_name,
                row.owner_name,
                row.team_name,
                row.decision_title,
                row.decision_summary,
                row.source_title,
                row.path.join(" ")
            )
            .to_ascii_lowercase();
            needles.iter().any(|needle| haystack.contains(needle))
        })
        .cloned()
        .take(20)
        .collect();
    if matched.is_empty() {
        rows.into_iter().take(20).collect()
    } else {
        matched
    }
}

fn ranked_context(question: &str, rows: Vec<ContextRecord>, plan: RetrievalPlan) -> Result<(Vec<ContextRecord>, RetrievalPlan, RetrievalResult), ToolError> {
    if plan.needs_clarification {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("company question needs clarification: {}", plan.ambiguity_reasons.join("; "))));
    }
    let candidates = relevant(question, rows);
    let evidence: Vec<EvidenceRecord> = candidates
        .iter()
        .map(|row| EvidenceRecord {
            id: record_key(row),
            title: row.source_title.clone(),
            text: format!("{} {} {} {} {} {} {}", row.ticket_title, row.project_name, row.owner_name, row.team_name, row.decision_title, row.decision_summary, row.source_title),
            source_id: row.source_id.clone(),
            source_uri: row.source_uri.clone(),
            path: row.path.clone(),
            updated_at: row.source_updated_at.parse::<i64>().unwrap_or(0) / 1_000,
            authority: match row.source_kind.to_ascii_lowercase().as_str() {
                "policy" | "record" => 100,
                "document" | "adr" => 95,
                "ticket" | "issue" => 85,
                "chat" | "message" => 60,
                _ => 50,
            },
            claims: evidence_claims(plan.template, row),
        })
        .collect();
    let result = company_retrieval::rank(&plan, evidence, chrono::Utc::now().timestamp());
    if let Some(reason) = company_retrieval::refusal_reason(&result) {
        let code = if result.status == company_retrieval::RetrievalStatus::Ambiguous { ErrorCode::InvalidArguments } else { ErrorCode::NotFound };
        return Err(ToolError::new(code, format!("not enough connected company evidence: {reason}")));
    }
    let order: BTreeMap<String, usize> = result.evidence.iter().enumerate().map(|(index, item)| (item.evidence.id.clone(), index)).collect();
    let mut selected: Vec<_> = candidates.into_iter().filter(|row| order.contains_key(&record_key(row))).collect();
    selected.sort_by_key(|row| order.get(&record_key(row)).copied().unwrap_or(usize::MAX));
    Ok((selected, plan, result))
}

fn evidence_claims(template: QueryTemplate, row: &ContextRecord) -> Vec<EvidenceClaim> {
    let claim = |subject: &str, predicate: &str, object: &str| EvidenceClaim { subject: subject.into(), predicate: predicate.into(), object: object.into() };
    match template {
        QueryTemplate::OwnershipPath => vec![claim(&row.ticket_id, "belongs_to", &row.project_id), claim(&row.project_id, "owned_by", &row.owner_id)],
        QueryTemplate::DecisionRationale => vec![claim(&row.decision_id, "affects", &row.project_id), claim(&row.decision_id, "rationale", &row.decision_summary)],
        QueryTemplate::ExpertisePath => vec![claim(&row.owner_id, "has_expertise", &row.ticket_id)],
        QueryTemplate::DependencyPath => vec![claim(&row.project_id, &row.decision_summary.to_ascii_lowercase(), &row.decision_id)],
        QueryTemplate::IncidentContext => vec![claim(&row.ticket_id, "belongs_to", &row.project_id)],
        QueryTemplate::ConnectedContext => vec![claim(&row.ticket_id, &row.decision_id.to_ascii_lowercase(), &row.project_id)],
    }
}

fn render(rows: &[ContextRecord]) -> String {
    if rows.is_empty() {
        return "FalkorDB returned no connected company context for this tenant.".into();
    }
    let paths: BTreeSet<_> = rows.iter().map(|row| format!("{} | {} | {} | {}", row.path.join(" -> "), row.source_kind, row.source_title, row.source_uri)).collect();
    format!(
        "Connected company context from FalkorDB:\n{}\nUse only these records for company-specific claims. Cite the numbered source mapping appended by Aqua and include graph source IDs when useful.",
        paths.into_iter().map(|path| format!("- {path}")).collect::<Vec<_>>().join("\n")
    )
}

fn structured(rows: &[ContextRecord]) -> Value {
    let paths: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "path": row.path
            })
        })
        .collect();
    let sources: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.source_id, "title": row.source_title, "kind": row.source_kind, "uri": row.source_uri
            })
        })
        .collect();
    json!({"records": rows, "paths": paths, "sources": sources, "graph_required": true})
}

pub struct CompanyProvider {
    graph: Arc<dyn CompanyGraph>,
    config: CompanyConfig,
}

struct FalkorMemoryBackend {
    graph: Arc<dyn CompanyGraph>,
    secrets: Arc<Mutex<Vec<String>>>,
}

impl CompanyMemoryBackend for FalkorMemoryBackend {
    fn execute(&self, statement: &GraphStatement) -> Result<GraphResult, CompanyMemoryError> {
        let response = self.graph.memory(statement).map_err(|error| CompanyMemoryError::Backend(error.to_string()))?;
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).extend(response.secrets);
        Ok(response.value)
    }
}

fn memory_repository(graph: Arc<dyn CompanyGraph>) -> (CompanyMemoryRepository<FalkorMemoryBackend>, Arc<Mutex<Vec<String>>>) {
    let secrets = Arc::new(Mutex::new(Vec::new()));
    (CompanyMemoryRepository::new(FalkorMemoryBackend { graph, secrets: secrets.clone() }), secrets)
}

fn memory_error(error: CompanyMemoryError) -> ToolError {
    match error {
        CompanyMemoryError::InvalidInput(message) => ToolError::new(ErrorCode::InvalidArguments, message),
        CompanyMemoryError::Unauthorized(message) => ToolError::new(ErrorCode::PermissionDenied, message),
        CompanyMemoryError::Backend(message) => ToolError::new(ErrorCode::BackendError, message),
    }
}

fn take_secrets(secrets: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    let mut values = std::mem::take(&mut *secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    values.sort();
    values.dedup();
    values
}

impl CompanyProvider {
    pub fn live(config: CompanyConfig, secrets: Arc<dyn Secrets>) -> Self {
        Self { graph: Arc::new(FalkorGraph { config: config.clone(), secrets }), config }
    }
}

impl ToolProvider for CompanyProvider {
    fn id(&self) -> &str {
        "company"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        vec![
            ToolManifest {
                name: SEARCH.into(),
                title: "Search company context".into(),
                description: "Follow company ownership, ticket, project, decision and source links in FalkorDB before answering a company question.".into(),
                classes: vec![PermClass::Read, PermClass::Network, PermClass::Remote, PermClass::Credential],
                default_decision: Decision::Auto,
                input_schema: json!({"type":"object","properties":{"question":{"type":"string","minLength":2,"maxLength":500}},"required":["question"]}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: false },
                untrusted_output: true,
            },
            ToolManifest {
                name: EXPLAIN.into(),
                title: "Explain a company graph path".into(),
                description: "Show the exact tenant-scoped ownership, decision and source path connected to one company entity.".into(),
                classes: vec![PermClass::Read, PermClass::Network, PermClass::Remote, PermClass::Credential],
                default_decision: Decision::Auto,
                input_schema: json!({"type":"object","properties":{"entity_id":{"type":"string","minLength":1,"maxLength":100}},"required":["entity_id"]}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: false },
                untrusted_output: true,
            },
            ToolManifest {
                name: PROPOSE.into(),
                title: "Propose a graph-backed assignment".into(),
                description: "Propose a ticket assignee only from the person connected as owner of that ticket's project; this does not mutate the graph.".into(),
                classes: vec![PermClass::Read, PermClass::Network, PermClass::Remote, PermClass::Credential],
                default_decision: Decision::Auto,
                input_schema: json!({"type":"object","properties":{"ticket_id":{"type":"string","minLength":1,"maxLength":100}},"required":["ticket_id"]}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: false },
                untrusted_output: true,
            },
            ToolManifest {
                name: ASSIGN.into(),
                title: "Assign a company ticket".into(),
                description: "Assign a ticket only when FalkorDB proves the person owns its project, then record the action and deciding path.".into(),
                classes: vec![PermClass::Write, PermClass::Network, PermClass::Remote, PermClass::Credential, PermClass::ExternalEffect],
                default_decision: Decision::AlwaysAsk,
                input_schema: json!({"type":"object","properties":{"ticket_id":{"type":"string","minLength":1,"maxLength":100},"assignee_id":{"type":"string","minLength":1,"maxLength":100}},"required":["ticket_id","assignee_id"]}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: false, destructive: false, idempotent: true, open_world: false },
                untrusted_output: false,
            },
            ToolManifest {
                name: RECORD_OUTCOME.into(),
                title: "Record a company action outcome".into(),
                description: "Append a succeeded or failed outcome to an existing tenant-scoped action without overwriting its history.".into(),
                classes: vec![PermClass::Write, PermClass::Network, PermClass::Remote, PermClass::Credential, PermClass::ExternalEffect],
                default_decision: Decision::AlwaysAsk,
                input_schema: json!({"type":"object","properties":{"action_id":{"type":"string","minLength":1,"maxLength":100},"status":{"type":"string","enum":["succeeded","failed"]},"summary":{"type":"string","minLength":1,"maxLength":240}},"required":["action_id","status","summary"]}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: false, destructive: false, idempotent: false, open_world: false },
                untrusted_output: false,
            },
            ToolManifest {
                name: INGEST.into(),
                title: "Ingest company records".into(),
                description: "Synchronize a bounded tenant-scoped company envelope into FalkorDB with provenance, freshness and deletion tracking.".into(),
                classes: vec![PermClass::Write, PermClass::Network, PermClass::Remote, PermClass::Credential, PermClass::ExternalEffect],
                default_decision: Decision::AlwaysAsk,
                input_schema: json!({"type":"object","properties":{"envelope":{"type":"object"},"stale_after_ms":{"type":"integer","minimum":60000,"maximum":31536000000_i64}},"required":["envelope"],"additionalProperties":false}),
                timeout_default_ms: 30_000,
                timeout_max_ms: 120_000,
                annotations: Annotations { read_only: false, destructive: false, idempotent: true, open_world: false },
                untrusted_output: false,
            },
            ToolManifest {
                name: OPEN_SESSION.into(),
                title: "Open a company agent session".into(),
                description: "Register an agent and open a tenant-scoped persistent session that another agent can later continue.".into(),
                classes: vec![PermClass::Write, PermClass::Network, PermClass::Remote, PermClass::Credential],
                default_decision: Decision::Ask,
                input_schema: json!({"type":"object","properties":{"agent_id":{"type":"string","maxLength":128},"agent_name":{"type":"string","maxLength":200},"agent_role":{"type":"string","maxLength":200},"session_id":{"type":"string","maxLength":128},"user_id":{"type":"string","maxLength":128}},"required":["agent_id","agent_name","agent_role","session_id","user_id"],"additionalProperties":false}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: false, destructive: false, idempotent: true, open_world: false },
                untrusted_output: false,
            },
            ToolManifest {
                name: SHARE_STATE.into(),
                title: "Share company agent state".into(),
                description: "Append a tenant-scoped state revision for other agents without overwriting prior revisions.".into(),
                classes: vec![PermClass::Write, PermClass::Network, PermClass::Remote, PermClass::Credential],
                default_decision: Decision::Ask,
                input_schema: json!({"type":"object","properties":{"agent_id":{"type":"string","maxLength":128},"revision_id":{"type":"string","maxLength":128},"key":{"type":"string","maxLength":128},"value":{},"previous_revision_id":{"type":"string","maxLength":128}},"required":["agent_id","revision_id","key","value"],"additionalProperties":false}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: false, destructive: false, idempotent: false, open_world: false },
                untrusted_output: false,
            },
            ToolManifest {
                name: HANDOFF.into(),
                title: "Hand off company work".into(),
                description: "Append an auditable tenant-scoped handoff from one registered agent to another in the same session.".into(),
                classes: vec![PermClass::Write, PermClass::Network, PermClass::Remote, PermClass::Credential],
                default_decision: Decision::Ask,
                input_schema: json!({"type":"object","properties":{"handoff_id":{"type":"string","maxLength":128},"session_id":{"type":"string","maxLength":128},"from_agent_id":{"type":"string","maxLength":128},"to_agent_id":{"type":"string","maxLength":128},"summary":{"type":"string","maxLength":32000}},"required":["handoff_id","session_id","from_agent_id","to_agent_id","summary"],"additionalProperties":false}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: false, destructive: false, idempotent: false, open_world: false },
                untrusted_output: false,
            },
            ToolManifest {
                name: RECALL_MEMORY.into(),
                title: "Recall company agent memory".into(),
                description: "Read a tenant-scoped prior session or the state revisions shared by a registered agent.".into(),
                classes: vec![PermClass::Read, PermClass::Network, PermClass::Remote, PermClass::Credential],
                default_decision: Decision::Auto,
                input_schema: json!({"type":"object","properties":{"kind":{"type":"string","enum":["session","shared_state"]},"id":{"type":"string","maxLength":128}},"required":["kind","id"],"additionalProperties":false}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: true, destructive: false, idempotent: true, open_world: false },
                untrusted_output: true,
            },
            ToolManifest {
                name: REMEMBER_EXCHANGE.into(),
                title: "Remember a company answer".into(),
                description: "Append a question, grounded answer and its source IDs to a persistent tenant-scoped agent session.".into(),
                classes: vec![PermClass::Write, PermClass::Network, PermClass::Remote, PermClass::Credential],
                default_decision: Decision::Ask,
                input_schema: json!({"type":"object","properties":{"agent_id":{"type":"string","maxLength":128},"session_id":{"type":"string","maxLength":128},"user_id":{"type":"string","maxLength":128},"question_id":{"type":"string","maxLength":128},"answer_id":{"type":"string","maxLength":128},"question":{"type":"string","maxLength":16000},"answer":{"type":"string","maxLength":64000},"source_ids":{"type":"array","maxItems":100,"items":{"type":"string","maxLength":128}}},"required":["agent_id","session_id","user_id","question_id","answer_id","question","answer","source_ids"],"additionalProperties":false}),
                timeout_default_ms: 20_000,
                timeout_max_ms: 30_000,
                annotations: Annotations { read_only: false, destructive: false, idempotent: false, open_world: false },
                untrusted_output: false,
            },
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        if ![SEARCH, EXPLAIN, PROPOSE, ASSIGN, RECORD_OUTCOME, INGEST, OPEN_SESSION, SHARE_STATE, HANDOFF, RECALL_MEMORY, REMEMBER_EXCHANGE].contains(&tool) {
            return Err(ToolError::new(ErrorCode::NotFound, format!("unknown company tool {tool}")));
        }
        let mut classes = vec![PermClass::Network];
        if !loopback(&self.config.host) {
            classes.push(PermClass::Remote);
        }
        if self.config.secret.is_some() {
            classes.push(PermClass::Credential);
        }
        let (target, summary) = if matches!(tool, OPEN_SESSION | SHARE_STATE | HANDOFF | REMEMBER_EXCHANGE) {
            classes.push(PermClass::Write);
            let id = match tool {
                OPEN_SESSION => args.get("session_id"),
                SHARE_STATE => args.get("revision_id"),
                REMEMBER_EXCHANGE => args.get("answer_id"),
                _ => args.get("handoff_id"),
            }
            .and_then(Value::as_str)
            .unwrap_or("");
            if !plain_id(id, 128) {
                return Err(ToolError::new(ErrorCode::InvalidArguments, "memory event identifier must be a plain identifier"));
            }
            (format!("{}:{}", self.config.tenant_id, id), format!("Append tenant-scoped company memory with {tool}"))
        } else if tool == RECALL_MEMORY {
            classes.push(PermClass::Read);
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
            if !plain_id(id, 128) || !matches!(kind, "session" | "shared_state") {
                return Err(ToolError::new(ErrorCode::InvalidArguments, "recall kind or id is invalid"));
            }
            (format!("{}:{}", self.config.tenant_id, id), format!("Recall tenant-scoped company {kind} memory"))
        } else if tool == INGEST {
            classes.extend([PermClass::Write, PermClass::ExternalEffect]);
            let envelope: IngestionEnvelope = serde_json::from_value(args.get("envelope").cloned().unwrap_or(Value::Null))
                .map_err(|error| ToolError::new(ErrorCode::InvalidArguments, format!("invalid ingestion envelope: {error}")))?;
            if envelope.tenant_id != self.config.tenant_id {
                return Err(ToolError::new(ErrorCode::PermissionDenied, "the ingestion envelope tenant does not match the configured tenant"));
            }
            (format!("{}:{}", envelope.tenant_id, envelope.source_system), format!("Synchronize {} company records from {}", envelope.records.len(), envelope.source_system))
        } else if tool == ASSIGN {
            classes.extend([PermClass::Write, PermClass::ExternalEffect]);
            let ticket = args.get("ticket_id").and_then(Value::as_str).unwrap_or("");
            let person = args.get("assignee_id").and_then(Value::as_str).unwrap_or("");
            if !plain_id(ticket, 100) || !plain_id(person, 100) {
                return Err(ToolError::new(ErrorCode::InvalidArguments, "ticket_id and assignee_id must be plain identifiers"));
            }
            (format!("{}:{}", self.config.tenant_id, ticket), format!("Assign {ticket} to {person} after FalkorDB validates project ownership"))
        } else if tool == RECORD_OUTCOME {
            classes.extend([PermClass::Write, PermClass::ExternalEffect]);
            let action = args.get("action_id").and_then(Value::as_str).unwrap_or("");
            let status = args.get("status").and_then(Value::as_str).unwrap_or("");
            let summary = args.get("summary").and_then(Value::as_str).unwrap_or("");
            if !plain_id(action, 100) || !matches!(status, "succeeded" | "failed") || !plain_text(summary, 240) {
                return Err(ToolError::new(ErrorCode::InvalidArguments, "action_id, status or summary is invalid"));
            }
            (format!("{}:{}", self.config.tenant_id, action), format!("Append a {status} outcome to company action {action}"))
        } else if tool == EXPLAIN {
            classes.push(PermClass::Read);
            let entity = args.get("entity_id").and_then(Value::as_str).unwrap_or("");
            if !plain_id(entity, 100) {
                return Err(ToolError::new(ErrorCode::InvalidArguments, "entity_id must be a plain identifier"));
            }
            (format!("{}:{}", self.config.tenant_id, entity), format!("Explain connected graph paths for {entity}"))
        } else if tool == PROPOSE {
            classes.push(PermClass::Read);
            let ticket = args.get("ticket_id").and_then(Value::as_str).unwrap_or("");
            if !plain_id(ticket, 100) {
                return Err(ToolError::new(ErrorCode::InvalidArguments, "ticket_id must be a plain identifier"));
            }
            (format!("{}:{}", self.config.tenant_id, ticket), format!("Find the graph-backed project owner for ticket {ticket}"))
        } else {
            classes.push(PermClass::Read);
            let question = args.get("question").and_then(Value::as_str).unwrap_or("").trim();
            if question.chars().count() < 2 {
                return Err(ToolError::new(ErrorCode::InvalidArguments, "question must contain at least two characters"));
            }
            (self.config.tenant_id.clone(), format!("Read connected company context for tenant {}", self.config.tenant_id))
        };
        Ok(Classification { classes, target, summary, target_allowed: true })
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let graph = self.graph.clone();
        let tool = tool.to_string();
        let request_id = ctx.request_id.clone();
        let tenant_id = self.config.tenant_id.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                if tool == SEARCH {
                    let question = args.get("question").and_then(Value::as_str).unwrap_or("");
                    let plan = company_retrieval::plan(question);
                    let response = graph.search(plan.template)?;
                    let (rows, plan, retrieval) = ranked_context(question, response.value, plan)?;
                    let mut data = structured(&rows);
                    if let Some(object) = data.as_object_mut() {
                        object.insert("retrieval".into(), json!({"intent": plan.intent, "template": plan.template.id(), "confidence": retrieval.confidence, "status": retrieval.status}));
                    }
                    Ok(ProviderOutput { data, text: render(&rows), secrets: response.secrets, private: true, ..Default::default() })
                } else if tool == EXPLAIN {
                    let entity = args.get("entity_id").and_then(Value::as_str).unwrap_or("");
                    let response = graph.search(QueryTemplate::ConnectedContext)?;
                    let rows: Vec<_> = response.value.into_iter().filter(|row| row.path.iter().any(|part| part == entity)).take(20).collect();
                    if rows.is_empty() {
                        return Err(ToolError::new(ErrorCode::NotFound, format!("no connected FalkorDB path contains entity {entity}")));
                    }
                    Ok(ProviderOutput { data: structured(&rows), text: render(&rows), secrets: response.secrets, private: true, ..Default::default() })
                } else if tool == PROPOSE {
                    let ticket = args.get("ticket_id").and_then(Value::as_str).unwrap_or("");
                    let response = graph.search(QueryTemplate::OwnershipPath)?;
                    let row = response.value.into_iter().find(|row| row.ticket_id == ticket)
                        .ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("no tenant-scoped owner path was found for ticket {ticket}")))?;
                    let path = format!("{} -> BELONGS_TO -> {} -> OWNED_BY -> {}", row.ticket_id, row.project_id, row.owner_id);
                    let evidence = row.clone();
                    Ok(ProviderOutput {
                        data: json!({"ticket_id": row.ticket_id, "project_id": row.project_id, "assignee_id": row.owner_id, "assignee_name": row.owner_name, "path": path, "source_id": row.source_id, "records": [evidence]}),
                        text: format!("Proposed {} for {} because FalkorDB returned path {}. Source: {}.", row.owner_name, ticket, path, row.source_id),
                        secrets: response.secrets, private: true, ..Default::default()
                    })
                } else if tool == ASSIGN {
                    let ticket = args.get("ticket_id").and_then(Value::as_str).unwrap_or("");
                    let assignee = args.get("assignee_id").and_then(Value::as_str).unwrap_or("");
                    let request_part: String = request_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(80).collect();
                    let action_id = if request_part.is_empty() { "action_aqua".into() } else { format!("action_{request_part}") };
                    let response = graph.assign(ticket, assignee, &action_id)?;
                    Ok(ProviderOutput { data: json!({"action_id": action_id, "ticket_id": ticket, "assignee_id": assignee, "graph_validated": true}), text: format!("Assigned {ticket} to {assignee}. FalkorDB validated the ownership path and recorded action {action_id}."), secrets: response.secrets, private: true, ..Default::default() })
                } else if tool == RECORD_OUTCOME {
                    let action = args.get("action_id").and_then(Value::as_str).unwrap_or("");
                    let status = args.get("status").and_then(Value::as_str).unwrap_or("");
                    let summary = args.get("summary").and_then(Value::as_str).unwrap_or("");
                    let request_part: String = request_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(80).collect();
                    let outcome_id = if request_part.is_empty() { "outcome_aqua".into() } else { format!("outcome_{request_part}") };
                    let response = graph.record_outcome(action, &outcome_id, status, summary)?;
                    Ok(ProviderOutput { data: json!({"action_id": action, "outcome_id": outcome_id, "status": status, "append_only": true}), text: format!("Recorded {status} outcome {outcome_id} for action {action}."), secrets: response.secrets, private: true, ..Default::default() })
                } else if tool == INGEST {
                    let envelope: IngestionEnvelope = serde_json::from_value(args.get("envelope").cloned().unwrap_or(Value::Null))
                        .map_err(|error| ToolError::new(ErrorCode::InvalidArguments, format!("invalid ingestion envelope: {error}")))?;
                    let stale_after_ms = args.get("stale_after_ms").and_then(Value::as_i64).unwrap_or(crate::company_ingest::DEFAULT_STALE_AFTER_MS);
                    if !(60_000..=31_536_000_000).contains(&stale_after_ms) {
                        return Err(ToolError::new(ErrorCode::InvalidArguments, "stale_after_ms must be between one minute and one year"));
                    }
                    let response = graph.ingest(envelope, stale_after_ms)?;
                    let (stats, status, mutation_count) = response.value;
                    Ok(ProviderOutput {
                        data: json!({"sync": stats, "status": status, "mutation_count": mutation_count, "graph_required": true}),
                        text: format!("Company ingestion completed through FalkorDB: {} received, {} upserted, {} unchanged, {} deactivated; {} active and {} stale records now tracked.", stats.received, stats.upserted, stats.unchanged, stats.deactivated, status.active_records, status.stale_records),
                        secrets: response.secrets, private: true, ..Default::default()
                    })
                } else if tool == OPEN_SESSION {
                    let scope = TenantScope::new(tenant_id.clone()).map_err(memory_error)?;
                    let agent = AgentMemory {
                        agent_id: args.get("agent_id").and_then(Value::as_str).unwrap_or("").into(),
                        name: args.get("agent_name").and_then(Value::as_str).unwrap_or("").into(),
                        role: args.get("agent_role").and_then(Value::as_str).unwrap_or("").into(),
                    };
                    let session = SessionMemory {
                        session_id: args.get("session_id").and_then(Value::as_str).unwrap_or("").into(),
                        agent_id: agent.agent_id.clone(),
                        user_id: args.get("user_id").and_then(Value::as_str).unwrap_or("").into(),
                    };
                    let (repository, held_secrets) = memory_repository(graph);
                    repository.register_agent(&scope, &agent).map_err(memory_error)?;
                    let result = repository.open_session(&scope, &session).map_err(memory_error)?;
                    Ok(ProviderOutput { data: json!({"session_id": session.session_id, "agent_id": agent.agent_id, "rows": result.rows, "persistent": true}), text: format!("Opened persistent company session {} for agent {}.", session.session_id, agent.agent_id), secrets: take_secrets(&held_secrets), private: true, ..Default::default() })
                } else if tool == SHARE_STATE {
                    let value = args.get("value").cloned().unwrap_or(Value::Null);
                    if value.to_string().len() > 64_000 { return Err(ToolError::new(ErrorCode::InvalidArguments, "shared state value exceeds 64000 characters")); }
                    let scope = TenantScope::new(tenant_id.clone()).map_err(memory_error)?;
                    let revision = SharedStateRevision {
                        revision_id: args.get("revision_id").and_then(Value::as_str).unwrap_or("").into(),
                        agent_id: args.get("agent_id").and_then(Value::as_str).unwrap_or("").into(),
                        key: args.get("key").and_then(Value::as_str).unwrap_or("").into(),
                        value,
                        previous_revision_id: args.get("previous_revision_id").and_then(Value::as_str).map(str::to_string),
                    };
                    let (repository, held_secrets) = memory_repository(graph);
                    let result = repository.append_shared_state(&scope, &revision).map_err(memory_error)?;
                    Ok(ProviderOutput { data: json!({"revision_id": revision.revision_id, "agent_id": revision.agent_id, "rows": result.rows, "append_only": true}), text: format!("Shared state revision {} from agent {} was appended.", revision.revision_id, revision.agent_id), secrets: take_secrets(&held_secrets), private: true, ..Default::default() })
                } else if tool == HANDOFF {
                    let scope = TenantScope::new(tenant_id.clone()).map_err(memory_error)?;
                    let handoff = HandoffMemory {
                        handoff_id: args.get("handoff_id").and_then(Value::as_str).unwrap_or("").into(),
                        session_id: args.get("session_id").and_then(Value::as_str).unwrap_or("").into(),
                        from_agent_id: args.get("from_agent_id").and_then(Value::as_str).unwrap_or("").into(),
                        to_agent_id: args.get("to_agent_id").and_then(Value::as_str).unwrap_or("").into(),
                        summary: args.get("summary").and_then(Value::as_str).unwrap_or("").into(),
                    };
                    let (repository, held_secrets) = memory_repository(graph);
                    let result = repository.handoff(&scope, &handoff).map_err(memory_error)?;
                    Ok(ProviderOutput { data: json!({"handoff_id": handoff.handoff_id, "session_id": handoff.session_id, "from_agent_id": handoff.from_agent_id, "to_agent_id": handoff.to_agent_id, "rows": result.rows}), text: format!("Recorded handoff {} from {} to {}.", handoff.handoff_id, handoff.from_agent_id, handoff.to_agent_id), secrets: take_secrets(&held_secrets), private: true, ..Default::default() })
                } else if tool == RECALL_MEMORY {
                    let scope = TenantScope::new(tenant_id.clone()).map_err(memory_error)?;
                    let id = args.get("id").and_then(Value::as_str).unwrap_or("");
                    let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
                    let (repository, held_secrets) = memory_repository(graph);
                    let result = if kind == "session" { repository.recall_session(&scope, id) } else { repository.recall_shared_state(&scope, id) }.map_err(memory_error)?;
                    Ok(ProviderOutput { data: json!({"kind": kind, "id": id, "events": result.rows, "persistent": true}), text: format!("Recalled {} tenant-scoped company memory rows for {kind} {id}.", result.rows.len()), secrets: take_secrets(&held_secrets), private: true, ..Default::default() })
                } else if tool == REMEMBER_EXCHANGE {
                    let scope = TenantScope::new(tenant_id.clone()).map_err(memory_error)?;
                    let agent_id = args.get("agent_id").and_then(Value::as_str).unwrap_or("").to_string();
                    let session_id = args.get("session_id").and_then(Value::as_str).unwrap_or("").to_string();
                    let question_id = args.get("question_id").and_then(Value::as_str).unwrap_or("").to_string();
                    let answer_id = args.get("answer_id").and_then(Value::as_str).unwrap_or("").to_string();
                    let source_ids: Vec<String> = serde_json::from_value(args.get("source_ids").cloned().unwrap_or(Value::Null))
                        .map_err(|error| ToolError::new(ErrorCode::InvalidArguments, format!("source_ids must be an array of identifiers: {error}")))?;
                    let agent = AgentMemory { agent_id: agent_id.clone(), name: "Aqua Company Agent".into(), role: "Company Brain".into() };
                    let session = SessionMemory {
                        session_id: session_id.clone(),
                        agent_id: agent_id.clone(),
                        user_id: args.get("user_id").and_then(Value::as_str).unwrap_or("").into(),
                    };
                    let question = QuestionMemory {
                        session_id: session_id.clone(),
                        agent_id: agent_id.clone(),
                        question_id: question_id.clone(),
                        question: args.get("question").and_then(Value::as_str).unwrap_or("").into(),
                    };
                    let answer = AnswerMemory {
                        agent_id: agent_id.clone(),
                        question_id,
                        answer_id: answer_id.clone(),
                        answer: args.get("answer").and_then(Value::as_str).unwrap_or("").into(),
                        source_ids,
                    };
                    let (repository, held_secrets) = memory_repository(graph);
                    repository.register_agent(&scope, &agent).map_err(memory_error)?;
                    repository.open_session(&scope, &session).map_err(memory_error)?;
                    repository.remember_question(&scope, &question).map_err(memory_error)?;
                    let result = repository.remember_answer(&scope, &answer).map_err(memory_error)?;
                    Ok(ProviderOutput { data: json!({"session_id": session_id, "agent_id": agent_id, "answer_id": answer_id, "source_ids": answer.source_ids, "rows": result.rows, "append_only": true}), text: format!("Remembered grounded answer {answer_id} in company session {session_id}."), secrets: take_secrets(&held_secrets), private: true, ..Default::default() })
                } else {
                    Err(ToolError::new(ErrorCode::NotFound, format!("unknown company tool {tool}")))
                }
            }).await.map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the Company Brain task stopped unexpectedly: {e}")))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_native_tools::secrets::MemorySecrets;
    use aqua_tool_host::{AuditLedger, Caller, Mode, PolicyEngine, RequestContext, ToolHost, ToolRequest};

    fn sample(source_id: &str, source_title: &str) -> ContextRecord {
        ContextRecord {
            ticket_id: "INC-1042".into(),
            ticket_title: "OAuth production incident".into(),
            project_id: "project_identity".into(),
            project_name: "Identity Platform".into(),
            owner_id: "person_priya".into(),
            owner_name: "Priya Raman".into(),
            owner_role: "Staff Engineer".into(),
            team_name: "Identity Engineering".into(),
            decision_id: "DEC-014".into(),
            decision_title: "Use staged PKCE migration".into(),
            decision_summary: "Roll out PKCE by client cohort.".into(),
            source_id: source_id.into(),
            source_title: source_title.into(),
            source_kind: "document".into(),
            source_uri: format!("company://{source_id}"),
            source_updated_at: "1760572800000".into(),
            path: vec!["INC-1042".into(), "BELONGS_TO".into(), "project_identity".into(), "OWNED_BY".into(), "person_priya".into(), "SUPPORTED_BY".into(), source_id.into()],
        }
    }

    struct FakeGraph;
    impl CompanyGraph for FakeGraph {
        fn search(&self, _template: QueryTemplate) -> Result<GraphResponse<Vec<ContextRecord>>, ToolError> {
            Ok(GraphResponse { value: vec![sample("SRC-1", "ADR-014"), sample("SRC-2", "Incident handoff")], secrets: vec![] })
        }
        fn assign(&self, ticket: &str, assignee: &str, _action: &str) -> Result<GraphResponse<()>, ToolError> {
            if ticket == "INC-1042" && assignee == "person_priya" {
                Ok(GraphResponse { value: (), secrets: vec![] })
            } else {
                Err(ToolError::new(ErrorCode::Conflict, "ownership was not proved"))
            }
        }
        fn record_outcome(&self, action: &str, _outcome: &str, status: &str, _summary: &str) -> Result<GraphResponse<()>, ToolError> {
            if action == "action_1" && matches!(status, "succeeded" | "failed") {
                Ok(GraphResponse { value: (), secrets: vec![] })
            } else {
                Err(ToolError::new(ErrorCode::NotFound, "action was not found"))
            }
        }
        fn ingest(&self, envelope: IngestionEnvelope, _stale_after_ms: i64) -> Result<GraphResponse<(SyncStats, IngestionStatus, usize)>, ToolError> {
            let received = envelope.records.len();
            Ok(GraphResponse {
                value: (
                    SyncStats { received, upserted: received, unchanged: 0, deactivated: 0 },
                    IngestionStatus { last_successful_sync_ms: Some(envelope.observed_at_ms), active_records: received, stale_records: 0, inactive_records: 0, error_count: 0 },
                    received,
                ),
                secrets: vec![],
            })
        }
        fn memory(&self, statement: &GraphStatement) -> Result<GraphResponse<GraphResult>, ToolError> {
            Ok(GraphResponse { value: GraphResult { rows: vec![[("statement".into(), json!(statement.name))].into_iter().collect()] }, secrets: vec![] })
        }
    }

    fn provider() -> CompanyProvider {
        CompanyProvider { graph: Arc::new(FakeGraph), config: CompanyConfig::default() }
    }

    #[test]
    fn config_requires_tls_and_a_secret_for_a_remote_enabled_graph() {
        let mut config = CompanyConfig { enabled: true, host: "db.example.com".into(), ..CompanyConfig::default() };
        assert!(config.validate().unwrap_err().contains("TLS"));
        config.tls = true;
        assert!(config.validate().unwrap_err().contains("secret"));
        config.secret = Some("company/falkordb".into());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn parses_the_scalar_shape_returned_by_graph_query() {
        let header = |name: &str| Reply::Array(Some(vec![Reply::Int(1), Reply::Bulk(Some(name.as_bytes().to_vec()))]));
        let value = |text: &str| Reply::Array(Some(vec![Reply::Int(2), Reply::Bulk(Some(text.as_bytes().to_vec()))]));
        let names = [
            "ticket_id",
            "ticket_title",
            "project_id",
            "project_name",
            "owner_id",
            "owner_name",
            "owner_role",
            "team_name",
            "decision_id",
            "decision_title",
            "decision_summary",
            "source_id",
            "source_title",
            "source_kind",
            "source_uri",
            "source_updated_at",
        ];
        let values = ["INC-1", "Incident", "P-1", "Platform", "U-1", "Priya", "Engineer", "Identity", "D-1", "Decision", "Reason", "S-1", "ADR", "document", "company://ADR", "1760572800000"];
        let row = Reply::Array(Some(values.iter().map(|v| value(v)).collect()));
        let reply = Reply::Array(Some(vec![Reply::Array(Some(names.iter().map(|n| header(n)).collect())), Reply::Array(Some(vec![row])), Reply::Array(Some(vec![]))]));
        let rows = parse_context_reply(&reply).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].ticket_id.as_str(), rows[0].owner_name.as_str(), rows[0].source_id.as_str()), ("INC-1", "Priya", "S-1"));
    }

    #[test]
    fn retrieval_templates_map_to_distinct_bounded_tenant_scoped_queries_and_paths() {
        let cases = [
            (QueryTemplate::OwnershipPath, "[:OWNS]", "OWNED_BY"),
            (QueryTemplate::DecisionRationale, "(decision:Decision)-[:AFFECTS]", "AFFECTS"),
            (QueryTemplate::ExpertisePath, "[:HAS_EXPERTISE]", "HAS_EXPERTISE"),
            (QueryTemplate::DependencyPath, ":DEPENDS_ON|USES", "DEPENDS_ON"),
            (QueryTemplate::IncidentContext, "<-[:OWNS]-", "OWNED_BY"),
            (QueryTemplate::ConnectedContext, "[connection]", "BELONGS_TO"),
        ];
        let mut queries = BTreeSet::new();
        for (template, marker, expected_path_marker) in cases {
            let query = query_for_template(template, "tenant-a");
            assert!(query.contains(marker), "{template:?}: {query}");
            assert!(query.contains("tenant-a") && query.contains("coalesce(source.active, true)"));
            assert!(query.contains("stale_at") && query.ends_with("LIMIT 100"));
            assert!(queries.insert(query), "each intent must use a distinct fixed query");

            let mut row = sample("SRC-1", "source");
            if template == QueryTemplate::DependencyPath {
                row.decision_summary = "DEPENDS_ON".into();
            } else if template == QueryTemplate::ConnectedContext {
                row.decision_id = "BELONGS_TO".into();
            }
            row.path = context_path(template, &row);
            assert!(row.path.iter().any(|part| part == expected_path_marker), "{template:?}: {:?}", row.path);
            assert_eq!(row.path.last().map(String::as_str), Some("SRC-1"));
        }
    }

    #[test]
    fn search_output_is_private_untrusted_and_carries_paths_and_sources() {
        let base = std::env::temp_dir().join(format!("aqua_company_brain_{}", std::process::id()));
        let host = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(base.join("audit.jsonl"))));
        host.register(Arc::new(provider())).unwrap();
        let mut context = RequestContext::new(Caller::Agent);
        context.agent_run_id = "company-run".into();
        let result = host.call_blocking(ToolRequest { tool: SEARCH.into(), arguments: json!({"question":"Who owns the OAuth incident and why?"}), context });
        assert!(result.ok, "{}", result.text);
        assert!(result.text.contains("BELONGS_TO") && result.text.contains("SUPPORTED_BY") && result.text.contains("SRC-1"), "{}", result.text);
        assert_eq!(result.data["paths"].as_array().map(Vec::len), Some(2));
        assert_eq!(result.data["sources"].as_array().map(Vec::len), Some(2));
        assert!(host.is_private("company-run") && host.is_tainted("company-run"));
    }

    #[test]
    fn assignment_is_always_an_external_effect_and_invalid_ids_are_refused() {
        let p = provider();
        let manifest = p.manifests().into_iter().find(|m| m.name == ASSIGN).unwrap();
        assert_eq!(manifest.default_decision, Decision::AlwaysAsk);
        assert!(manifest.classes.contains(&PermClass::ExternalEffect));
        let error = p.classify(ASSIGN, &json!({"ticket_id":"x' MATCH (n)","assignee_id":"person_priya"})).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArguments);
        let outcome = p.manifests().into_iter().find(|m| m.name == RECORD_OUTCOME).unwrap();
        assert_eq!(outcome.default_decision, Decision::AlwaysAsk);
        assert!(!outcome.annotations.idempotent);
        assert!(p.classify(RECORD_OUTCOME, &json!({"action_id":"action_1","status":"succeeded","summary":"Deployed safely"})).is_ok());
        assert!(p.classify(RECORD_OUTCOME, &json!({"action_id":"action_1","status":"failed","summary":"x' MATCH n"})).is_err());
        let _ = MemorySecrets::new(); // The live provider accepts only the Secrets interface; tests never touch Credential Manager.
    }
}
