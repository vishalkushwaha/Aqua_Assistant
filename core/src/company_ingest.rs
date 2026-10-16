//! Normalization and deterministic synchronization for Company Brain data.
//!
//! This module deliberately performs no I/O. Tool providers may feed it bytes they have obtained
//! through `ToolHost`, then execute the returned parameterized mutations through the Company Brain
//! graph backend. Keeping adapters and planning pure makes retries deterministic and prevents an
//! importer from becoming a second door to the filesystem or FalkorDB.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const DEFAULT_STALE_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_RECORDS: usize = 20_000;
const MAX_TEXT_BYTES: usize = 1_000_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Person,
    Team,
    Project,
    Ticket,
    Decision,
    Document,
    ChatMessage,
    System,
    Topic,
}

impl EntityKind {
    pub fn graph_label(self) -> &'static str {
        match self {
            Self::Person => "Person",
            Self::Team => "Team",
            Self::Project => "Project",
            Self::Ticket => "Ticket",
            Self::Decision => "Decision",
            Self::Document => "Document",
            Self::ChatMessage => "ChatMessage",
            Self::System => "System",
            Self::Topic => "Topic",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    MemberOf,
    Owns,
    BelongsTo,
    AssignedTo,
    Affects,
    AuthoredBy,
    References,
    HasExpertise,
    WorkedOn,
    Uses,
    DependsOn,
}

impl RelationKind {
    pub fn graph_label(self) -> &'static str {
        match self {
            Self::MemberOf => "MEMBER_OF",
            Self::Owns => "OWNS",
            Self::BelongsTo => "BELONGS_TO",
            Self::AssignedTo => "ASSIGNED_TO",
            Self::Affects => "AFFECTS",
            Self::AuthoredBy => "AUTHORED_BY",
            Self::References => "REFERENCES",
            Self::HasExpertise => "HAS_EXPERTISE",
            Self::WorkedOn => "WORKED_ON",
            Self::Uses => "USES",
            Self::DependsOn => "DEPENDS_ON",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Relation {
    pub kind: RelationKind,
    pub target_kind: EntityKind,
    pub target_external_id: String,
    /// Omit when both records came from the same source system.
    #[serde(default)]
    pub target_source_system: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordInput {
    pub kind: EntityKind,
    pub external_id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub source_updated_at_ms: Option<i64>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub attributes: BTreeMap<String, Value>,
    #[serde(default)]
    pub relations: Vec<Relation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IngestionEnvelope {
    pub tenant_id: String,
    pub source_system: String,
    pub source_id: String,
    pub source_uri: String,
    pub observed_at_ms: i64,
    #[serde(default)]
    pub complete_snapshot: bool,
    pub records: Vec<RecordInput>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EntityKey {
    pub tenant_id: String,
    pub source_system: String,
    pub kind: EntityKind,
    pub external_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NormalizedRecord {
    pub key: EntityKey,
    pub source_id: String,
    pub source_uri: String,
    pub title: String,
    pub body: String,
    pub content_hash: String,
    pub observed_at_ms: i64,
    pub source_updated_at_ms: Option<i64>,
    pub stale_at_ms: i64,
    pub active: bool,
    pub attributes: BTreeMap<String, Value>,
    pub relations: Vec<Relation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StoredRecord {
    pub key: EntityKey,
    pub content_hash: String,
    pub active: bool,
    pub observed_at_ms: i64,
    pub stale_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncReason {
    New,
    Changed,
    Reactivated,
    Unchanged,
    ExplicitDeletion,
    MissingFromSnapshot,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SyncOperation {
    Upsert { record: NormalizedRecord, reason: SyncReason },
    Touch { key: EntityKey, observed_at_ms: i64, stale_at_ms: i64 },
    Deactivate { key: EntityKey, observed_at_ms: i64, reason: SyncReason },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SyncStats {
    pub received: usize,
    pub upserted: usize,
    pub unchanged: usize,
    pub deactivated: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncPlan {
    pub operations: Vec<SyncOperation>,
    pub stats: SyncStats,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct IngestionStatus {
    pub last_successful_sync_ms: Option<i64>,
    pub active_records: usize,
    pub stale_records: usize,
    pub inactive_records: usize,
    pub error_count: usize,
}

pub fn ingestion_status(records: &[StoredRecord], now_ms: i64, error_count: usize) -> IngestionStatus {
    let mut status = IngestionStatus { error_count, ..IngestionStatus::default() };
    for record in records {
        status.last_successful_sync_ms = Some(status.last_successful_sync_ms.map_or(record.observed_at_ms, |last| last.max(record.observed_at_ms)));
        if !record.active {
            status.inactive_records += 1;
        } else if record.stale_at_ms <= now_ms {
            status.stale_records += 1;
        } else {
            status.active_records += 1;
        }
    }
    status
}

/// A fixed Cypher template plus separately encoded parameters. Labels come only from enums; user
/// text is never interpolated into the query.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphMutation {
    pub query: String,
    pub parameters: BTreeMap<String, Value>,
}

fn plain_id(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/'))
}

fn normalized_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn hash_record(record: &RecordInput, title: &str, body: &str, relations: &[Relation]) -> Result<String, String> {
    let canonical = json!({
        "attributes": record.attributes,
        "body": body,
        "kind": record.kind,
        "relations": relations,
        "title": title,
    });
    let bytes = serde_json::to_vec(&canonical).map_err(|e| format!("could not serialize normalized record: {e}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub fn normalize(envelope: IngestionEnvelope, stale_after_ms: i64) -> Result<Vec<NormalizedRecord>, String> {
    if !plain_id(&envelope.tenant_id, 80) {
        return Err("tenant_id must be a plain identifier of at most 80 characters".into());
    }
    if !plain_id(&envelope.source_system, 80) || !plain_id(&envelope.source_id, 160) {
        return Err("source_system and source_id must be plain identifiers".into());
    }
    if envelope.source_uri.trim().is_empty() || envelope.source_uri.len() > 2_048 {
        return Err("source_uri must contain 1-2048 characters".into());
    }
    if envelope.observed_at_ms <= 0 || stale_after_ms <= 0 {
        return Err("observed_at_ms and stale_after_ms must be positive".into());
    }
    if envelope.records.len() > MAX_RECORDS {
        return Err(format!("an ingestion envelope may contain at most {MAX_RECORDS} records"));
    }

    let mut seen = BTreeSet::new();
    let mut normalized = Vec::with_capacity(envelope.records.len());
    for mut record in envelope.records {
        record.external_id = record.external_id.trim().to_string();
        if !plain_id(&record.external_id, 200) {
            return Err(format!("external_id {:?} is not a plain identifier", record.external_id));
        }
        let key = EntityKey { tenant_id: envelope.tenant_id.clone(), source_system: envelope.source_system.clone(), kind: record.kind, external_id: record.external_id.clone() };
        if !seen.insert(key.clone()) {
            return Err(format!("duplicate record {:?}", record.external_id));
        }
        if record.body.len() > MAX_TEXT_BYTES {
            return Err(format!("record {:?} exceeds the text size limit", record.external_id));
        }
        if record.source_updated_at_ms.is_some_and(|at| at <= 0 || at > envelope.observed_at_ms) {
            return Err(format!("record {:?} has an invalid source_updated_at_ms", record.external_id));
        }
        for relation in &mut record.relations {
            relation.target_external_id = relation.target_external_id.trim().to_string();
            if !plain_id(&relation.target_external_id, 200) {
                return Err(format!("record {:?} has an invalid relation target", record.external_id));
            }
            if relation.target_source_system.as_ref().is_some_and(|source| !plain_id(source, 80)) {
                return Err(format!("record {:?} has an invalid relation target source", record.external_id));
            }
        }
        record.relations.sort();
        record.relations.dedup();
        let title = normalized_text(&record.title);
        if !record.deleted && title.is_empty() {
            return Err(format!("record {:?} needs a title", record.external_id));
        }
        let body = record.body.trim().to_string();
        let content_hash = hash_record(&record, &title, &body, &record.relations)?;
        normalized.push(NormalizedRecord {
            key,
            source_id: envelope.source_id.clone(),
            source_uri: envelope.source_uri.clone(),
            title,
            body,
            content_hash,
            observed_at_ms: envelope.observed_at_ms,
            source_updated_at_ms: record.source_updated_at_ms,
            stale_at_ms: envelope.observed_at_ms.saturating_add(stale_after_ms),
            active: !record.deleted,
            attributes: record.attributes,
            relations: record.relations,
        });
    }
    normalized.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(normalized)
}

pub fn plan_sync(envelope: IngestionEnvelope, previous: &[StoredRecord], stale_after_ms: i64) -> Result<SyncPlan, String> {
    let tenant = envelope.tenant_id.clone();
    let source = envelope.source_system.clone();
    let observed_at_ms = envelope.observed_at_ms;
    let complete_snapshot = envelope.complete_snapshot;
    let records = normalize(envelope, stale_after_ms)?;
    let prior: BTreeMap<_, _> = previous.iter().filter(|p| p.key.tenant_id == tenant && p.key.source_system == source).map(|p| (p.key.clone(), p)).collect();
    if prior.len() != previous.len() {
        return Err("previous records must belong to the envelope tenant and source system".into());
    }
    let incoming: BTreeSet<_> = records.iter().map(|r| r.key.clone()).collect();
    let mut operations = Vec::new();
    let mut stats = SyncStats { received: records.len(), ..SyncStats::default() };
    for record in records {
        if !record.active {
            operations.push(SyncOperation::Deactivate { key: record.key, observed_at_ms, reason: SyncReason::ExplicitDeletion });
            stats.deactivated += 1;
            continue;
        }
        let reason = match prior.get(&record.key) {
            None => Some(SyncReason::New),
            Some(old) if !old.active => Some(SyncReason::Reactivated),
            Some(old) if old.content_hash != record.content_hash => Some(SyncReason::Changed),
            Some(_) => None,
        };
        match reason {
            Some(reason) => {
                operations.push(SyncOperation::Upsert { record, reason });
                stats.upserted += 1;
            }
            None => {
                operations.push(SyncOperation::Touch { key: record.key, observed_at_ms, stale_at_ms: record.stale_at_ms });
                stats.unchanged += 1;
            }
        }
    }
    if complete_snapshot {
        for old in prior.values().filter(|old| old.active && !incoming.contains(&old.key)) {
            operations.push(SyncOperation::Deactivate { key: old.key.clone(), observed_at_ms, reason: SyncReason::MissingFromSnapshot });
            stats.deactivated += 1;
        }
    }
    operations.sort_by(|a, b| operation_key(a).cmp(&operation_key(b)));
    Ok(SyncPlan { operations, stats })
}

fn operation_key(operation: &SyncOperation) -> (&EntityKey, u8) {
    match operation {
        SyncOperation::Upsert { record, .. } => (&record.key, 0),
        SyncOperation::Touch { key, .. } => (key, 1),
        SyncOperation::Deactivate { key, .. } => (key, 2),
    }
}

pub fn compile_mutations(plan: &SyncPlan) -> Vec<GraphMutation> {
    let mut mutations = Vec::new();
    // Nodes are always created before edges. The normalized record order is deterministic, but a
    // Person can sort before the Team or Project it references.
    for operation in &plan.operations {
        match operation {
            SyncOperation::Upsert { record, .. } => {
                let label = record.key.kind.graph_label();
                let mut parameters = base_parameters(&record.key);
                let role = record.attributes.get("role").and_then(Value::as_str).unwrap_or_default();
                parameters.extend([
                    ("source_id".into(), json!(record.source_id)),
                    ("source_uri".into(), json!(record.source_uri)),
                    ("title".into(), json!(record.title)),
                    ("body".into(), json!(record.body)),
                    ("content_hash".into(), json!(record.content_hash)),
                    ("observed_at_ms".into(), json!(record.observed_at_ms)),
                    ("source_updated_at_ms".into(), json!(record.source_updated_at_ms)),
                    ("stale_at_ms".into(), json!(record.stale_at_ms)),
                    ("attributes_json".into(), json!(serde_json::to_string(&record.attributes).unwrap_or_else(|_| "{}".into()))),
                    ("role".into(), json!(role)),
                ]);
                mutations.push(GraphMutation {
                    query: format!("MERGE (n:{label} {{tenant_id: $tenant_id, source_system: $source_system, external_id: $external_id}}) SET n.id = $external_id, n.source_id = $source_id, n.source_uri = $source_uri, n.title = $title, n.name = $title, n.body = $body, n.summary = $body, n.role = $role, n.content_hash = $content_hash, n.observed_at = $observed_at_ms, n.source_updated_at = $source_updated_at_ms, n.stale_at = $stale_at_ms, n.attributes_json = $attributes_json, n.active = true RETURN n.external_id AS external_id"),
                    parameters,
                });
            }
            SyncOperation::Touch { key, observed_at_ms, stale_at_ms } => mutations.push(GraphMutation {
                query: format!("MATCH (n:{} {{tenant_id: $tenant_id, source_system: $source_system, external_id: $external_id}}) SET n.observed_at = $observed_at_ms, n.stale_at = $stale_at_ms RETURN n.external_id AS external_id", key.kind.graph_label()),
                parameters: base_parameters(key).into_iter().chain([("observed_at_ms".into(), json!(observed_at_ms)), ("stale_at_ms".into(), json!(stale_at_ms))]).collect(),
            }),
            SyncOperation::Deactivate { key, observed_at_ms, .. } => mutations.push(GraphMutation {
                query: format!("MATCH (n:{} {{tenant_id: $tenant_id, source_system: $source_system, external_id: $external_id}}) SET n.active = false, n.observed_at = $observed_at_ms, n.deactivated_at = $observed_at_ms WITH n OPTIONAL MATCH (n)-[r]->() WHERE r.managed_by = 'company_ingest' SET r.active = false RETURN n.external_id AS external_id", key.kind.graph_label()),
                parameters: base_parameters(key).into_iter().chain([("observed_at_ms".into(), json!(observed_at_ms))]).collect(),
            }),
        }
    }
    for operation in &plan.operations {
        let SyncOperation::Upsert { record, .. } = operation else { continue };
        let label = record.key.kind.graph_label();
        mutations.push(GraphMutation {
            query: format!("MATCH (n:{label} {{tenant_id: $tenant_id, source_system: $source_system, external_id: $external_id}})-[r]->() WHERE r.managed_by = 'company_ingest' DELETE r RETURN n.external_id AS external_id"),
            parameters: base_parameters(&record.key),
        });
        let mut source_parameters = base_parameters(&record.key);
        source_parameters.extend([
            ("source_id".into(), json!(record.source_id)),
            ("source_uri".into(), json!(record.source_uri)),
            ("observed_at_ms".into(), json!(record.observed_at_ms)),
            ("source_updated_at_ms".into(), json!(record.source_updated_at_ms)),
        ]);
        mutations.push(GraphMutation {
            query: format!("MATCH (n:{label} {{tenant_id: $tenant_id, source_system: $source_system, external_id: $external_id}}) MERGE (source:Source {{tenant_id: $tenant_id, source_system: $source_system, id: $source_id}}) SET source.title = $source_id, source.kind = $source_system, source.uri = $source_uri, source.observed_at = $observed_at_ms, source.updated_at = $source_updated_at_ms, source.active = true MERGE (n)-[r:SUPPORTED_BY]->(source) SET r.managed_by = 'company_ingest', r.active = true RETURN n.external_id AS external_id"),
            parameters: source_parameters,
        });
        for relation in &record.relations {
            let mut relation_parameters = base_parameters(&record.key);
            relation_parameters.insert("target_source_system".into(), json!(relation.target_source_system.as_deref().unwrap_or(&record.key.source_system)));
            relation_parameters.insert("target_external_id".into(), json!(relation.target_external_id));
            mutations.push(GraphMutation {
                query: format!("MATCH (from:{label} {{tenant_id: $tenant_id, source_system: $source_system, external_id: $external_id}}) MATCH (to:{} {{tenant_id: $tenant_id, source_system: $target_source_system, external_id: $target_external_id}}) MERGE (from)-[r:{}]->(to) SET r.managed_by = 'company_ingest', r.active = true RETURN from.external_id AS external_id", relation.target_kind.graph_label(), relation.kind.graph_label()),
                parameters: relation_parameters,
            });
        }
    }
    mutations
}

fn base_parameters(key: &EntityKey) -> BTreeMap<String, Value> {
    [("tenant_id".into(), json!(key.tenant_id)), ("source_system".into(), json!(key.source_system)), ("external_id".into(), json!(key.external_id))].into_iter().collect()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkdownOptions {
    pub tenant_id: String,
    pub source_system: String,
    pub source_id: String,
    pub source_uri: String,
    pub observed_at_ms: i64,
    #[serde(default)]
    pub source_updated_at_ms: Option<i64>,
    #[serde(default)]
    pub project_ids: Vec<String>,
}

pub fn markdown_envelope(markdown: &str, options: MarkdownOptions) -> Result<IngestionEnvelope, String> {
    if markdown.len() > MAX_TEXT_BYTES {
        return Err("Markdown document exceeds the text size limit".into());
    }
    let parsed = crate::front_matter::parse(markdown)?;
    let external_id = parsed.scalar("id").unwrap_or(&options.source_id).to_string();
    let title = parsed
        .scalar("title")
        .map(str::to_string)
        .or_else(|| parsed.body.lines().find_map(|line| line.trim().strip_prefix("# ").map(str::trim).map(str::to_string)))
        .unwrap_or_else(|| external_id.clone());
    let mut projects = options.project_ids;
    if let Some(list) = parsed.get("projects") {
        projects.extend(list.items());
    }
    projects.sort();
    projects.dedup();
    let relations =
        projects.into_iter().map(|target_external_id| Relation { kind: RelationKind::References, target_kind: EntityKind::Project, target_external_id, target_source_system: None }).collect();
    Ok(IngestionEnvelope {
        tenant_id: options.tenant_id,
        source_system: options.source_system,
        source_id: options.source_id,
        source_uri: options.source_uri,
        observed_at_ms: options.observed_at_ms,
        complete_snapshot: false,
        records: vec![RecordInput {
            kind: EntityKind::Document,
            external_id,
            title,
            body: parsed.body,
            source_updated_at_ms: options.source_updated_at_ms,
            deleted: false,
            attributes: BTreeMap::new(),
            relations,
        }],
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonExport {
    tenant_id: String,
    source: ExportSource,
    observed_at_ms: i64,
    #[serde(default)]
    complete_snapshot: bool,
    #[serde(default)]
    people: Vec<PersonExport>,
    #[serde(default)]
    teams: Vec<NamedExport>,
    #[serde(default)]
    projects: Vec<ProjectExport>,
    #[serde(default)]
    systems: Vec<SystemExport>,
    #[serde(default)]
    topics: Vec<NamedExport>,
    #[serde(default)]
    tickets: Vec<TicketExport>,
    #[serde(default)]
    decisions: Vec<DecisionExport>,
    #[serde(default)]
    chats: Vec<ChatExport>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportSource {
    system: String,
    id: String,
    uri: String,
}

#[derive(Debug, Deserialize)]
struct CommonExport {
    id: String,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    updated_at_ms: Option<i64>,
    #[serde(default)]
    deleted: bool,
}

#[derive(Debug, Deserialize)]
struct NamedExport {
    #[serde(flatten)]
    common: CommonExport,
}

#[derive(Debug, Deserialize)]
struct PersonExport {
    #[serde(flatten)]
    common: CommonExport,
    #[serde(default)]
    role: String,
    #[serde(default)]
    team_ids: Vec<String>,
    #[serde(default)]
    owns_project_ids: Vec<String>,
    #[serde(default)]
    expertise_topic_ids: Vec<String>,
    #[serde(default)]
    worked_on_project_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProjectExport {
    #[serde(flatten)]
    common: CommonExport,
    #[serde(default)]
    uses_system_ids: Vec<String>,
    #[serde(default)]
    depends_on_project_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SystemExport {
    #[serde(flatten)]
    common: CommonExport,
    #[serde(default)]
    depends_on_system_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TicketExport {
    #[serde(flatten)]
    common: CommonExport,
    project_id: String,
    #[serde(default)]
    assignee_id: Option<String>,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Deserialize)]
struct DecisionExport {
    #[serde(flatten)]
    common: CommonExport,
    project_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ChatExport {
    #[serde(flatten)]
    common: CommonExport,
    author_id: String,
    #[serde(default)]
    project_ids: Vec<String>,
    #[serde(default)]
    channel: String,
}

pub fn json_export_envelope(bytes: &[u8]) -> Result<IngestionEnvelope, String> {
    if bytes.len() > 10 * MAX_TEXT_BYTES {
        return Err("JSON export exceeds the size limit".into());
    }
    let export: JsonExport = serde_json::from_slice(bytes).map_err(|e| format!("invalid Company Brain JSON export: {e}"))?;
    let mut records = Vec::new();
    records.extend(export.teams.into_iter().map(|v| common_record(EntityKind::Team, v.common, vec![], BTreeMap::new())));
    records.extend(export.topics.into_iter().map(|v| common_record(EntityKind::Topic, v.common, vec![], BTreeMap::new())));
    records.extend(export.systems.into_iter().map(|v| {
        let relations = v.depends_on_system_ids.into_iter().map(|id| relation(RelationKind::DependsOn, EntityKind::System, id)).collect();
        common_record(EntityKind::System, v.common, relations, BTreeMap::new())
    }));
    records.extend(export.projects.into_iter().map(|v| {
        let relations = v
            .uses_system_ids
            .into_iter()
            .map(|id| relation(RelationKind::Uses, EntityKind::System, id))
            .chain(v.depends_on_project_ids.into_iter().map(|id| relation(RelationKind::DependsOn, EntityKind::Project, id)))
            .collect();
        common_record(EntityKind::Project, v.common, relations, BTreeMap::new())
    }));
    records.extend(export.people.into_iter().map(|v| {
        let relations = v
            .team_ids
            .into_iter()
            .map(|id| relation(RelationKind::MemberOf, EntityKind::Team, id))
            .chain(v.owns_project_ids.into_iter().map(|id| relation(RelationKind::Owns, EntityKind::Project, id)))
            .chain(v.expertise_topic_ids.into_iter().map(|id| relation(RelationKind::HasExpertise, EntityKind::Topic, id)))
            .chain(v.worked_on_project_ids.into_iter().map(|id| relation(RelationKind::WorkedOn, EntityKind::Project, id)))
            .collect();
        common_record(EntityKind::Person, v.common, relations, [("role".into(), json!(v.role))].into_iter().collect())
    }));
    records.extend(export.tickets.into_iter().map(|v| {
        let mut relations = vec![relation(RelationKind::BelongsTo, EntityKind::Project, v.project_id)];
        if let Some(id) = v.assignee_id {
            relations.push(relation(RelationKind::AssignedTo, EntityKind::Person, id));
        }
        common_record(EntityKind::Ticket, v.common, relations, [("status".into(), json!(v.status))].into_iter().collect())
    }));
    records.extend(export.decisions.into_iter().map(|v| {
        let relations = v.project_ids.into_iter().map(|id| relation(RelationKind::Affects, EntityKind::Project, id)).collect();
        common_record(EntityKind::Decision, v.common, relations, BTreeMap::new())
    }));
    records.extend(export.chats.into_iter().map(|v| {
        let relations = [relation(RelationKind::AuthoredBy, EntityKind::Person, v.author_id)]
            .into_iter()
            .chain(v.project_ids.into_iter().map(|id| relation(RelationKind::References, EntityKind::Project, id)))
            .collect();
        common_record(EntityKind::ChatMessage, v.common, relations, [("channel".into(), json!(v.channel))].into_iter().collect())
    }));
    Ok(IngestionEnvelope {
        tenant_id: export.tenant_id,
        source_system: export.source.system,
        source_id: export.source.id,
        source_uri: export.source.uri,
        observed_at_ms: export.observed_at_ms,
        complete_snapshot: export.complete_snapshot,
        records,
    })
}

fn relation(kind: RelationKind, target_kind: EntityKind, target_external_id: String) -> Relation {
    Relation { kind, target_kind, target_external_id, target_source_system: None }
}

fn common_record(kind: EntityKind, common: CommonExport, relations: Vec<Relation>, attributes: BTreeMap<String, Value>) -> RecordInput {
    RecordInput { kind, external_id: common.id, title: common.title, body: common.body, source_updated_at_ms: common.updated_at_ms, deleted: common.deleted, attributes, relations }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, title: &str) -> RecordInput {
        RecordInput {
            kind: EntityKind::Ticket,
            external_id: id.into(),
            title: title.into(),
            body: "body".into(),
            source_updated_at_ms: Some(900),
            deleted: false,
            attributes: BTreeMap::new(),
            relations: vec![],
        }
    }

    fn envelope(records: Vec<RecordInput>) -> IngestionEnvelope {
        IngestionEnvelope {
            tenant_id: "tenant-a".into(),
            source_system: "tracker".into(),
            source_id: "export-1".into(),
            source_uri: "https://tracker/export/1".into(),
            observed_at_ms: 1_000,
            complete_snapshot: true,
            records,
        }
    }

    #[test]
    fn unchanged_records_touch_without_duplicate_upsert() {
        let first = normalize(envelope(vec![record("T-1", "Fix login")]), 500).unwrap().remove(0);
        let previous = StoredRecord { key: first.key.clone(), content_hash: first.content_hash, active: true, observed_at_ms: 900, stale_at_ms: 1_400 };
        let plan = plan_sync(envelope(vec![record("T-1", "Fix login")]), &[previous], 500).unwrap();
        assert_eq!(plan.stats, SyncStats { received: 1, upserted: 0, unchanged: 1, deactivated: 0 });
        assert!(matches!(plan.operations[0], SyncOperation::Touch { stale_at_ms: 1_500, .. }));
    }

    #[test]
    fn snapshot_deactivates_missing_and_explicitly_deleted_records() {
        let key = |id: &str| EntityKey { tenant_id: "tenant-a".into(), source_system: "tracker".into(), kind: EntityKind::Ticket, external_id: id.into() };
        let previous = vec![
            StoredRecord { key: key("gone"), content_hash: "a".into(), active: true, observed_at_ms: 1, stale_at_ms: 10 },
            StoredRecord { key: key("deleted"), content_hash: "b".into(), active: true, observed_at_ms: 1, stale_at_ms: 10 },
        ];
        let mut deleted = record("deleted", "Deleted");
        deleted.deleted = true;
        let plan = plan_sync(envelope(vec![deleted]), &previous, 500).unwrap();
        assert_eq!(plan.stats.deactivated, 2);
        assert!(plan.operations.iter().any(|op| matches!(op, SyncOperation::Deactivate { reason: SyncReason::ExplicitDeletion, .. })));
        assert!(plan.operations.iter().any(|op| matches!(op, SyncOperation::Deactivate { reason: SyncReason::MissingFromSnapshot, .. })));
    }

    #[test]
    fn cross_tenant_previous_state_is_rejected() {
        let previous = StoredRecord {
            key: EntityKey { tenant_id: "tenant-b".into(), source_system: "tracker".into(), kind: EntityKind::Ticket, external_id: "T-1".into() },
            content_hash: "x".into(),
            active: true,
            observed_at_ms: 1,
            stale_at_ms: 10,
        };
        assert!(plan_sync(envelope(vec![record("T-1", "Fix login")]), &[previous], 500).unwrap_err().contains("tenant"));
    }

    #[test]
    fn markdown_adapter_preserves_source_and_project_connections() {
        let input = "---\nid: doc-7\ntitle: Decision log\nprojects: [alpha, beta]\n---\n# ignored heading\nUse the regional queue.";
        let env = markdown_envelope(
            input,
            MarkdownOptions {
                tenant_id: "t".into(),
                source_system: "docs".into(),
                source_id: "wiki-7".into(),
                source_uri: "https://wiki/7".into(),
                observed_at_ms: 100,
                source_updated_at_ms: Some(90),
                project_ids: vec!["alpha".into()],
            },
        )
        .unwrap();
        assert_eq!(env.records[0].external_id, "doc-7");
        assert_eq!(env.records[0].relations.len(), 2);
        assert_eq!(env.records[0].body, "# ignored heading\nUse the regional queue.");
    }

    #[test]
    fn json_adapter_connects_people_tickets_chats_and_projects() {
        let input = br#"{
          "tenant_id":"acme","source":{"system":"demo","id":"oct","uri":"file:///export.json"},
          "observed_at_ms":2000,"complete_snapshot":true,
          "teams":[{"id":"platform","title":"Platform"}],
          "projects":[{"id":"alpha","title":"Alpha"}],
          "people":[{"id":"ava","title":"Ava","role":"lead","team_ids":["platform"],"owns_project_ids":["alpha"]}],
          "tickets":[{"id":"T-1","title":"Outage","project_id":"alpha","assignee_id":"ava","status":"open"}],
          "decisions":[{"id":"D-1","title":"Use queues","project_ids":["alpha"]}],
          "chats":[{"id":"M-1","title":"Incident note","body":"Queue is saturated","author_id":"ava","project_ids":["alpha"],"channel":"incidents"}]
        }"#;
        let env = json_export_envelope(input).unwrap();
        let rows = normalize(env.clone(), DEFAULT_STALE_AFTER_MS).unwrap();
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|r| r.key.tenant_id == "acme"));
        let ticket = rows.iter().find(|r| r.key.kind == EntityKind::Ticket).unwrap();
        assert_eq!(ticket.relations.len(), 2);
        let plan = plan_sync(env, &[], DEFAULT_STALE_AFTER_MS).unwrap();
        let mutations = compile_mutations(&plan);
        let last_node = mutations.iter().rposition(|m| m.query.starts_with("MERGE (n:")).unwrap();
        let first_relation = mutations.iter().position(|m| m.query.contains("MEMBER_OF")).unwrap();
        assert!(last_node < first_relation, "all target nodes must exist before relationship writes");
    }

    #[test]
    fn json_adapter_builds_expertise_work_history_and_dependency_edges() {
        let input = br#"{
          "tenant_id":"acme","source":{"system":"demo","id":"oct","uri":"file:///export.json"},
          "observed_at_ms":2000,
          "topics":[{"id":"falkordb","title":"FalkorDB performance"}],
          "systems":[{"id":"redis","title":"Redis","depends_on_system_ids":[]}],
          "projects":[{"id":"brain","title":"Company Brain","uses_system_ids":["redis"],"depends_on_project_ids":["identity"]},{"id":"identity","title":"Identity"}],
          "people":[{"id":"maya","title":"Maya","expertise_topic_ids":["falkordb"],"worked_on_project_ids":["brain"]}]
        }"#;
        let env = json_export_envelope(input).unwrap();
        let plan = plan_sync(env, &[], DEFAULT_STALE_AFTER_MS).unwrap();
        let mutations = compile_mutations(&plan);
        for relation in ["HAS_EXPERTISE", "WORKED_ON", "USES", "DEPENDS_ON"] {
            let mutation = mutations.iter().find(|mutation| mutation.query.contains(&format!("[r:{relation}]"))).unwrap_or_else(|| panic!("missing {relation}"));
            assert!(mutation.query.contains("tenant_id: $tenant_id"));
            assert!(mutation.query.contains("managed_by = 'company_ingest'"));
            assert!(!mutation.query.contains("maya") && !mutation.query.contains("brain"));
        }
    }

    #[test]
    fn mutations_are_parameterized_and_staleness_is_explicit() {
        let plan = plan_sync(envelope(vec![record("T-1", "Robert'); MATCH (n) DETACH DELETE n //")]), &[], 500).unwrap();
        let mutations = compile_mutations(&plan);
        let mutation = &mutations[0];
        assert!(!mutation.query.contains("Robert"));
        assert_eq!(mutation.parameters["title"], json!("Robert'); MATCH (n) DETACH DELETE n //"));
        assert_eq!(mutation.parameters["stale_at_ms"], json!(1_500));
        assert!(mutation.query.contains("active = true"));
        assert!(mutations.iter().any(|m| m.query.contains("SUPPORTED_BY")));
    }

    #[test]
    fn status_separates_fresh_stale_and_inactive_records() {
        let key = |id: &str| EntityKey { tenant_id: "tenant-a".into(), source_system: "tracker".into(), kind: EntityKind::Ticket, external_id: id.into() };
        let rows = vec![
            StoredRecord { key: key("fresh"), content_hash: "a".into(), active: true, observed_at_ms: 90, stale_at_ms: 110 },
            StoredRecord { key: key("stale"), content_hash: "b".into(), active: true, observed_at_ms: 80, stale_at_ms: 99 },
            StoredRecord { key: key("gone"), content_hash: "c".into(), active: false, observed_at_ms: 70, stale_at_ms: 90 },
        ];
        assert_eq!(ingestion_status(&rows, 100, 2), IngestionStatus { last_successful_sync_ms: Some(90), active_records: 1, stale_records: 1, inactive_records: 1, error_count: 2 });
    }
}
