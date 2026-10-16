//! Persistent, tenant-isolated memory primitives for Company Brain agents.
//!
//! This module deliberately does not open a database connection. It owns the fixed graph
//! statements and the invariants around them; the Company Brain provider can adapt
//! [`CompanyMemoryBackend`] to FalkorDB while tests and offline callers use another backend.
//! Event history is append-only. The only destructive operations are the explicit retention
//! and tenant-erasure methods, both of which require [`DeletionAuthorization`].

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

pub type GraphRow = BTreeMap<String, Value>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphOperation {
    Read,
    Append,
    Delete,
}

/// A fixed query plus separately-bound values. Backends must bind `params`; they must never
/// interpolate values into `cypher`.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphStatement {
    pub name: &'static str,
    pub cypher: &'static str,
    pub params: BTreeMap<String, Value>,
    pub operation: GraphOperation,
}

impl GraphStatement {
    fn new(name: &'static str, cypher: &'static str, tenant_id: &str, operation: GraphOperation) -> Result<Self, CompanyMemoryError> {
        validate_id("tenant_id", tenant_id)?;
        let mut params = BTreeMap::new();
        params.insert("tenant_id".into(), json!(tenant_id));
        Ok(Self { name, cypher, params, operation })
    }

    fn id(mut self, name: &'static str, value: &str) -> Result<Self, CompanyMemoryError> {
        validate_id(name, value)?;
        self.params.insert(name.into(), json!(value));
        Ok(self)
    }

    fn text(mut self, name: &'static str, value: &str, max: usize) -> Result<Self, CompanyMemoryError> {
        if value.trim().is_empty() || value.len() > max {
            return Err(CompanyMemoryError::InvalidInput(format!("{name} must contain 1-{max} characters")));
        }
        self.params.insert(name.into(), json!(value));
        Ok(self)
    }

    fn value(mut self, name: &'static str, value: Value) -> Self {
        self.params.insert(name.into(), value);
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphResult {
    pub rows: Vec<GraphRow>,
}

impl GraphResult {
    pub fn empty() -> Self {
        Self { rows: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompanyMemoryError {
    InvalidInput(String),
    Unauthorized(String),
    Backend(String),
}

impl fmt::Display for CompanyMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(f, "invalid company memory input: {message}"),
            Self::Unauthorized(message) => write!(f, "company memory authorization failed: {message}"),
            Self::Backend(message) => write!(f, "company memory backend failed: {message}"),
        }
    }
}

impl Error for CompanyMemoryError {}

pub trait CompanyMemoryBackend: Send + Sync {
    fn execute(&self, statement: &GraphStatement) -> Result<GraphResult, CompanyMemoryError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantScope {
    tenant_id: String,
}

impl TenantScope {
    pub fn new(tenant_id: impl Into<String>) -> Result<Self, CompanyMemoryError> {
        let tenant_id = tenant_id.into();
        validate_id("tenant_id", &tenant_id)?;
        Ok(Self { tenant_id })
    }

    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
}

/// Proof supplied by the caller after ToolHost has approved a destructive tenant operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletionAuthorization {
    tenant_id: String,
    approved_by: String,
    request_id: String,
}

impl DeletionAuthorization {
    pub fn new(scope: &TenantScope, approved_by: impl Into<String>, request_id: impl Into<String>) -> Result<Self, CompanyMemoryError> {
        let approved_by = approved_by.into();
        let request_id = request_id.into();
        validate_id("approved_by", &approved_by)?;
        validate_id("request_id", &request_id)?;
        Ok(Self { tenant_id: scope.tenant_id.clone(), approved_by, request_id })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentMemory {
    pub agent_id: String,
    pub name: String,
    pub role: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionMemory {
    pub session_id: String,
    pub agent_id: String,
    pub user_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct QuestionMemory {
    pub session_id: String,
    pub agent_id: String,
    pub question_id: String,
    pub question: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AnswerMemory {
    pub agent_id: String,
    pub question_id: String,
    pub answer_id: String,
    pub answer: String,
    pub source_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ActionMemory {
    pub session_id: String,
    pub agent_id: String,
    pub action_id: String,
    pub kind: String,
    pub target_id: String,
    pub input: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OutcomeMemory {
    pub action_id: String,
    pub outcome_id: String,
    pub status: String,
    pub detail: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FeedbackMemory {
    pub answer_id: String,
    pub feedback_id: String,
    pub author_id: String,
    pub rating: i8,
    pub comment: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SharedStateRevision {
    pub revision_id: String,
    pub agent_id: String,
    pub key: String,
    pub value: Value,
    pub previous_revision_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HandoffMemory {
    pub handoff_id: String,
    pub session_id: String,
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub summary: String,
}

pub struct CompanyMemoryRepository<B> {
    backend: B,
}

impl<B: CompanyMemoryBackend> CompanyMemoryRepository<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub fn register_agent(&self, scope: &TenantScope, memory: &AgentMemory) -> Result<GraphResult, CompanyMemoryError> {
        let statement = GraphStatement::new("company_memory.register_agent", REGISTER_AGENT, scope.tenant_id(), GraphOperation::Append)?
            .id("agent_id", &memory.agent_id)?
            .text("name", &memory.name, 200)?
            .text("role", &memory.role, 200)?;
        self.backend.execute(&statement)
    }

    pub fn open_session(&self, scope: &TenantScope, memory: &SessionMemory) -> Result<GraphResult, CompanyMemoryError> {
        let statement = GraphStatement::new("company_memory.open_session", OPEN_SESSION, scope.tenant_id(), GraphOperation::Append)?
            .id("session_id", &memory.session_id)?
            .id("agent_id", &memory.agent_id)?
            .id("user_id", &memory.user_id)?;
        self.backend.execute(&statement)
    }

    pub fn remember_question(&self, scope: &TenantScope, memory: &QuestionMemory) -> Result<GraphResult, CompanyMemoryError> {
        let statement = GraphStatement::new("company_memory.remember_question", REMEMBER_QUESTION, scope.tenant_id(), GraphOperation::Append)?
            .id("session_id", &memory.session_id)?
            .id("agent_id", &memory.agent_id)?
            .id("question_id", &memory.question_id)?
            .text("question", &memory.question, 16_000)?;
        self.backend.execute(&statement)
    }

    pub fn remember_answer(&self, scope: &TenantScope, memory: &AnswerMemory) -> Result<GraphResult, CompanyMemoryError> {
        let source_ids = validated_ids("source_ids", &memory.source_ids)?;
        let statement = GraphStatement::new("company_memory.remember_answer", REMEMBER_ANSWER, scope.tenant_id(), GraphOperation::Append)?
            .id("agent_id", &memory.agent_id)?
            .id("question_id", &memory.question_id)?
            .id("answer_id", &memory.answer_id)?
            .text("answer", &memory.answer, 64_000)?
            .value("source_ids", json!(source_ids));
        self.backend.execute(&statement)
    }

    pub fn remember_action(&self, scope: &TenantScope, memory: &ActionMemory) -> Result<GraphResult, CompanyMemoryError> {
        let statement = GraphStatement::new("company_memory.remember_action", REMEMBER_ACTION, scope.tenant_id(), GraphOperation::Append)?
            .id("session_id", &memory.session_id)?
            .id("agent_id", &memory.agent_id)?
            .id("action_id", &memory.action_id)?
            .id("kind", &memory.kind)?
            .id("target_id", &memory.target_id)?
            .value("input_json", json!(memory.input.to_string()));
        self.backend.execute(&statement)
    }

    pub fn remember_outcome(&self, scope: &TenantScope, memory: &OutcomeMemory) -> Result<GraphResult, CompanyMemoryError> {
        let statement = GraphStatement::new("company_memory.remember_outcome", REMEMBER_OUTCOME, scope.tenant_id(), GraphOperation::Append)?
            .id("action_id", &memory.action_id)?
            .id("outcome_id", &memory.outcome_id)?
            .id("status", &memory.status)?
            .value("detail_json", json!(memory.detail.to_string()));
        self.backend.execute(&statement)
    }

    pub fn remember_feedback(&self, scope: &TenantScope, memory: &FeedbackMemory) -> Result<GraphResult, CompanyMemoryError> {
        if !(-1..=1).contains(&memory.rating) {
            return Err(CompanyMemoryError::InvalidInput("rating must be -1, 0, or 1".into()));
        }
        let statement = GraphStatement::new("company_memory.remember_feedback", REMEMBER_FEEDBACK, scope.tenant_id(), GraphOperation::Append)?
            .id("answer_id", &memory.answer_id)?
            .id("feedback_id", &memory.feedback_id)?
            .id("author_id", &memory.author_id)?
            .text("comment", &memory.comment, 8_000)?
            .value("rating", json!(memory.rating));
        self.backend.execute(&statement)
    }

    pub fn append_shared_state(&self, scope: &TenantScope, revision: &SharedStateRevision) -> Result<GraphResult, CompanyMemoryError> {
        let mut statement = GraphStatement::new("company_memory.append_shared_state", APPEND_SHARED_STATE, scope.tenant_id(), GraphOperation::Append)?
            .id("revision_id", &revision.revision_id)?
            .id("agent_id", &revision.agent_id)?
            .id("key", &revision.key)?
            .value("value_json", json!(revision.value.to_string()));
        if let Some(previous) = &revision.previous_revision_id {
            statement = statement.id("previous_revision_id", previous)?.value("has_previous", json!(true));
        } else {
            statement = statement.value("previous_revision_id", Value::Null).value("has_previous", json!(false));
        }
        self.backend.execute(&statement)
    }

    pub fn handoff(&self, scope: &TenantScope, handoff: &HandoffMemory) -> Result<GraphResult, CompanyMemoryError> {
        let statement = GraphStatement::new("company_memory.handoff", HANDOFF, scope.tenant_id(), GraphOperation::Append)?
            .id("handoff_id", &handoff.handoff_id)?
            .id("session_id", &handoff.session_id)?
            .id("from_agent_id", &handoff.from_agent_id)?
            .id("to_agent_id", &handoff.to_agent_id)?
            .text("summary", &handoff.summary, 32_000)?;
        self.backend.execute(&statement)
    }

    pub fn recall_session(&self, scope: &TenantScope, session_id: &str) -> Result<GraphResult, CompanyMemoryError> {
        let statement = GraphStatement::new("company_memory.recall_session", RECALL_SESSION, scope.tenant_id(), GraphOperation::Read)?.id("session_id", session_id)?;
        self.backend.execute(&statement)
    }

    pub fn recall_shared_state(&self, scope: &TenantScope, agent_id: &str) -> Result<GraphResult, CompanyMemoryError> {
        let statement = GraphStatement::new("company_memory.recall_shared_state", RECALL_SHARED_STATE, scope.tenant_id(), GraphOperation::Read)?.id("agent_id", agent_id)?;
        self.backend.execute(&statement)
    }

    pub fn record_retention_policy(&self, scope: &TenantScope, revision_id: &str, max_age_days: u32) -> Result<GraphResult, CompanyMemoryError> {
        if !(1..=3_650).contains(&max_age_days) {
            return Err(CompanyMemoryError::InvalidInput("max_age_days must be between 1 and 3650".into()));
        }
        let statement = GraphStatement::new("company_memory.record_retention_policy", RETENTION_POLICY, scope.tenant_id(), GraphOperation::Append)?
            .id("revision_id", revision_id)?
            .value("max_age_days", json!(max_age_days));
        self.backend.execute(&statement)
    }

    pub fn prune_before(&self, scope: &TenantScope, before_epoch_ms: i64, auth: &DeletionAuthorization) -> Result<GraphResult, CompanyMemoryError> {
        authorize(scope, auth)?;
        if before_epoch_ms <= 0 {
            return Err(CompanyMemoryError::InvalidInput("before_epoch_ms must be positive".into()));
        }
        let statement = GraphStatement::new("company_memory.prune_before", PRUNE_BEFORE, scope.tenant_id(), GraphOperation::Delete)?
            .id("approved_by", &auth.approved_by)?
            .id("request_id", &auth.request_id)?
            .value("before_epoch_ms", json!(before_epoch_ms));
        self.backend.execute(&statement)
    }

    pub fn erase_tenant(&self, scope: &TenantScope, auth: &DeletionAuthorization) -> Result<GraphResult, CompanyMemoryError> {
        authorize(scope, auth)?;
        let statement =
            GraphStatement::new("company_memory.erase_tenant", ERASE_TENANT, scope.tenant_id(), GraphOperation::Delete)?.id("approved_by", &auth.approved_by)?.id("request_id", &auth.request_id)?;
        self.backend.execute(&statement)
    }

    pub fn into_backend(self) -> B {
        self.backend
    }
}

fn authorize(scope: &TenantScope, auth: &DeletionAuthorization) -> Result<(), CompanyMemoryError> {
    if auth.tenant_id != scope.tenant_id {
        return Err(CompanyMemoryError::Unauthorized("approval belongs to a different tenant".into()));
    }
    Ok(())
}

fn validate_id(name: &str, value: &str) -> Result<(), CompanyMemoryError> {
    if value.is_empty() || value.len() > 128 || !value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')) {
        return Err(CompanyMemoryError::InvalidInput(format!("{name} must be 1-128 letters, digits, _, -, . or :")));
    }
    Ok(())
}

fn validated_ids(name: &str, values: &[String]) -> Result<Vec<String>, CompanyMemoryError> {
    if values.len() > 100 {
        return Err(CompanyMemoryError::InvalidInput(format!("{name} has more than 100 values")));
    }
    for value in values {
        validate_id(name, value)?;
    }
    Ok(values.to_vec())
}

const OPEN_SESSION: &str = r#"
MERGE (agent:Agent {tenant_id: $tenant_id, id: $agent_id})
MERGE (session:Session {tenant_id: $tenant_id, id: $session_id})
  ON CREATE SET session.user_id = $user_id, session.created_at = timestamp()
MERGE (agent)-[:PARTICIPATED_IN]->(session)
RETURN session.id AS session_id
"#;

const REGISTER_AGENT: &str = r#"
MERGE (agent:Agent {tenant_id: $tenant_id, id: $agent_id})
  ON CREATE SET agent.name = $name, agent.role = $role, agent.created_at = timestamp()
RETURN agent.id AS agent_id
"#;

const REMEMBER_QUESTION: &str = r#"
MATCH (session:Session {tenant_id: $tenant_id, id: $session_id})
MATCH (agent:Agent {tenant_id: $tenant_id, id: $agent_id})
CREATE (question:Question {tenant_id: $tenant_id, id: $question_id, text: $question, created_at: timestamp()})
CREATE (agent)-[:ASKED]->(question)
CREATE (session)-[:CONTAINS]->(question)
RETURN question.id AS question_id
"#;

const REMEMBER_ANSWER: &str = r#"
MATCH (question:Question {tenant_id: $tenant_id, id: $question_id})
MATCH (agent:Agent {tenant_id: $tenant_id, id: $agent_id})
CREATE (answer:Answer {tenant_id: $tenant_id, id: $answer_id, text: $answer, source_ids: $source_ids, created_at: timestamp()})
CREATE (question)-[:ANSWERED_BY]->(answer)
CREATE (agent)-[:PRODUCED]->(answer)
RETURN question.id AS question_id, answer.id AS answer_id
"#;

const REMEMBER_ACTION: &str = r#"
MATCH (session:Session {tenant_id: $tenant_id, id: $session_id})
MATCH (agent:Agent {tenant_id: $tenant_id, id: $agent_id})
CREATE (action:Action {tenant_id: $tenant_id, id: $action_id, kind: $kind, target_id: $target_id, input_json: $input_json, created_at: timestamp()})
CREATE (session)-[:CONTAINS]->(action)
CREATE (agent)-[:PERFORMED]->(action)
RETURN action.id AS action_id
"#;

const REMEMBER_OUTCOME: &str = r#"
MATCH (action:Action {tenant_id: $tenant_id, id: $action_id})
CREATE (outcome:Outcome {tenant_id: $tenant_id, id: $outcome_id, status: $status, detail_json: $detail_json, created_at: timestamp()})
CREATE (action)-[:RESULTED_IN]->(outcome)
RETURN outcome.id AS outcome_id
"#;

const REMEMBER_FEEDBACK: &str = r#"
MATCH (answer:Answer {tenant_id: $tenant_id, id: $answer_id})
CREATE (feedback:Feedback {tenant_id: $tenant_id, id: $feedback_id, author_id: $author_id, rating: $rating, comment: $comment, created_at: timestamp()})
CREATE (feedback)-[:ABOUT]->(answer)
RETURN feedback.id AS feedback_id
"#;

const APPEND_SHARED_STATE: &str = r#"
MATCH (agent:Agent {tenant_id: $tenant_id, id: $agent_id})
OPTIONAL MATCH (previous:StateRevision {tenant_id: $tenant_id, id: $previous_revision_id})
WHERE $has_previous = true
CREATE (revision:StateRevision {tenant_id: $tenant_id, id: $revision_id, key: $key, value_json: $value_json, created_at: timestamp()})
CREATE (agent)-[:PUBLISHED_STATE]->(revision)
FOREACH (_ IN CASE WHEN previous IS NULL THEN [] ELSE [1] END | CREATE (revision)-[:PREVIOUS]->(previous))
RETURN revision.id AS revision_id
"#;

const HANDOFF: &str = r#"
MATCH (session:Session {tenant_id: $tenant_id, id: $session_id})
MATCH (from:Agent {tenant_id: $tenant_id, id: $from_agent_id})
MATCH (to:Agent {tenant_id: $tenant_id, id: $to_agent_id})
CREATE (handoff:Handoff {tenant_id: $tenant_id, id: $handoff_id, summary: $summary, created_at: timestamp()})
CREATE (from)-[:HANDED_OFF]->(handoff)
CREATE (handoff)-[:TO]->(to)
CREATE (session)-[:CONTAINS]->(handoff)
RETURN handoff.id AS handoff_id
"#;

const RECALL_SESSION: &str = r#"
MATCH (session:Session {tenant_id: $tenant_id, id: $session_id})-[:CONTAINS]->(event)
OPTIONAL MATCH (event)-[relation]->(related)
WHERE related.tenant_id = $tenant_id
RETURN labels(event) AS labels, properties(event) AS event, type(relation) AS relation, properties(related) AS related
ORDER BY event.created_at ASC
"#;

const RECALL_SHARED_STATE: &str = r#"
MATCH (agent:Agent {tenant_id: $tenant_id, id: $agent_id})-[:PUBLISHED_STATE]->(revision:StateRevision {tenant_id: $tenant_id})
RETURN revision.id AS revision_id, revision.key AS key, revision.value_json AS value_json, revision.created_at AS created_at
ORDER BY revision.created_at DESC
"#;

const RETENTION_POLICY: &str = r#"
MERGE (tenant:Tenant {tenant_id: $tenant_id})
CREATE (policy:RetentionPolicyRevision {tenant_id: $tenant_id, id: $revision_id, max_age_days: $max_age_days, created_at: timestamp()})
CREATE (tenant)-[:HAS_RETENTION_POLICY]->(policy)
RETURN policy.id AS revision_id
"#;

const PRUNE_BEFORE: &str = r#"
CREATE (run:DeletionRun {tenant_id: $tenant_id, id: $request_id, kind: 'retention', approved_by: $approved_by, cutoff: $before_epoch_ms, created_at: timestamp()})
WITH run
MATCH (event)
WHERE event.tenant_id = $tenant_id AND event.created_at < $before_epoch_ms
  AND (event:Question OR event:Answer OR event:Action OR event:Outcome OR event:Feedback OR event:Handoff OR event:StateRevision)
WITH run, collect(event) AS expired
UNWIND expired AS event
DETACH DELETE event
RETURN run.id AS request_id, size(expired) AS deleted_count
"#;

const ERASE_TENANT: &str = r#"
MATCH (node)
WHERE node.tenant_id = $tenant_id
WITH collect(node) AS doomed, count(node) AS deleted_count
UNWIND doomed AS node
DETACH DELETE node
WITH deleted_count
CREATE (receipt:TenantDeletionReceipt {tenant_id: $tenant_id, id: $request_id, approved_by: $approved_by, deleted_count: deleted_count, created_at: timestamp()})
RETURN receipt.id AS request_id, receipt.deleted_count AS deleted_count
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeBackend {
        statements: Mutex<Vec<GraphStatement>>,
    }

    impl CompanyMemoryBackend for FakeBackend {
        fn execute(&self, statement: &GraphStatement) -> Result<GraphResult, CompanyMemoryError> {
            assert!(statement.params.contains_key("tenant_id"), "every operation is tenant-scoped");
            assert!(statement.cypher.contains("$tenant_id"), "every query uses its tenant binding");
            self.statements.lock().unwrap().push(statement.clone());
            Ok(GraphResult::empty())
        }
    }

    fn scope(id: &str) -> TenantScope {
        TenantScope::new(id).unwrap()
    }

    #[test]
    fn answer_history_is_append_only_and_values_are_bound() {
        let repository = CompanyMemoryRepository::new(FakeBackend::default());
        let question = QuestionMemory { session_id: "session-1".into(), agent_id: "researcher".into(), question_id: "q-1".into(), question: "Who owns O'Brien's project?".into() };
        let answer = AnswerMemory {
            agent_id: "researcher".into(),
            question_id: "q-1".into(),
            answer_id: "a-1".into(),
            answer: "Ada owns it; MATCH (n) DELETE n is just text.".into(),
            source_ids: vec!["doc-7".into()],
        };
        repository.remember_question(&scope("tenant-a"), &question).unwrap();
        repository.remember_answer(&scope("tenant-a"), &answer).unwrap();
        let backend = repository.into_backend();
        let statements = backend.statements.lock().unwrap();
        assert_eq!(statements[0].operation, GraphOperation::Append);
        assert!(statements[0].cypher.contains("CREATE (question:Question"));
        assert!(statements[1].cypher.contains("CREATE (answer:Answer"));
        assert!(!statements[1].cypher.contains(" SET answer."));
        assert!(!statements[0].cypher.contains(&question.question));
        assert_eq!(statements[1].params["answer"], json!(answer.answer));
    }

    #[test]
    fn session_recall_and_shared_state_are_tenant_scoped() {
        let repository = CompanyMemoryRepository::new(FakeBackend::default());
        let tenant = scope("tenant-a");
        repository.recall_session(&tenant, "session-1").unwrap();
        repository
            .append_shared_state(
                &tenant,
                &SharedStateRevision {
                    revision_id: "state-2".into(),
                    agent_id: "planner".into(),
                    key: "incident-status".into(),
                    value: json!({"status": "triaged"}),
                    previous_revision_id: Some("state-1".into()),
                },
            )
            .unwrap();
        let backend = repository.into_backend();
        let statements = backend.statements.lock().unwrap();
        assert!(statements.iter().all(|s| s.params["tenant_id"] == json!("tenant-a")));
        assert_eq!(statements[1].params["previous_revision_id"], json!("state-1"));
        assert!(statements[1].cypher.contains(":PREVIOUS"));
    }

    #[test]
    fn handoff_connects_two_agents_without_overwriting_state() {
        let repository = CompanyMemoryRepository::new(FakeBackend::default());
        repository
            .handoff(
                &scope("tenant-a"),
                &HandoffMemory {
                    handoff_id: "handoff-1".into(),
                    session_id: "session-1".into(),
                    from_agent_id: "researcher".into(),
                    to_agent_id: "operator".into(),
                    summary: "Owner verified from doc-7".into(),
                },
            )
            .unwrap();
        let backend = repository.into_backend();
        let statements = backend.statements.lock().unwrap();
        let statement = &statements[0];
        assert!(statement.cypher.contains("CREATE (handoff:Handoff"));
        assert!(statement.cypher.contains("[:HANDED_OFF]"));
        assert!(statement.cypher.contains("[:TO]"));
        assert!(!statement.cypher.contains(" SET handoff."));
    }

    #[test]
    fn actions_outcomes_and_feedback_are_distinct_immutable_events() {
        let repository = CompanyMemoryRepository::new(FakeBackend::default());
        let tenant = scope("tenant-a");
        repository
            .remember_action(
                &tenant,
                &ActionMemory {
                    session_id: "session-1".into(),
                    agent_id: "operator".into(),
                    action_id: "action-1".into(),
                    kind: "assign_ticket".into(),
                    target_id: "ticket-1".into(),
                    input: json!({"assignee": "person-1"}),
                },
            )
            .unwrap();
        repository.remember_outcome(&tenant, &OutcomeMemory { action_id: "action-1".into(), outcome_id: "outcome-1".into(), status: "completed".into(), detail: json!({"changed": true}) }).unwrap();
        repository
            .remember_feedback(
                &tenant,
                &FeedbackMemory { answer_id: "answer-1".into(), feedback_id: "feedback-1".into(), author_id: "user-1".into(), rating: 1, comment: "Useful and well sourced".into() },
            )
            .unwrap();
        let backend = repository.into_backend();
        let statements = backend.statements.lock().unwrap();
        assert_eq!(
            statements.iter().map(|statement| statement.name).collect::<Vec<_>>(),
            vec!["company_memory.remember_action", "company_memory.remember_outcome", "company_memory.remember_feedback"]
        );
        assert!(statements.iter().all(|statement| statement.operation == GraphOperation::Append));
        assert_eq!(statements[0].params["input_json"], json!(r#"{"assignee":"person-1"}"#));
        assert_eq!(statements[1].params["detail_json"], json!(r#"{"changed":true}"#));
    }

    #[test]
    fn deletion_approval_cannot_cross_tenants() {
        let repository = CompanyMemoryRepository::new(FakeBackend::default());
        let tenant_a = scope("tenant-a");
        let tenant_b = scope("tenant-b");
        let approval = DeletionAuthorization::new(&tenant_a, "admin-1", "request-1").unwrap();
        let error = repository.erase_tenant(&tenant_b, &approval).unwrap_err();
        assert!(matches!(error, CompanyMemoryError::Unauthorized(_)));
        assert!(repository.into_backend().statements.lock().unwrap().is_empty());
    }

    #[test]
    fn retention_deletion_is_explicit_scoped_and_audited() {
        let repository = CompanyMemoryRepository::new(FakeBackend::default());
        let tenant = scope("tenant-a");
        let approval = DeletionAuthorization::new(&tenant, "admin-1", "purge-1").unwrap();
        repository.record_retention_policy(&tenant, "policy-2", 90).unwrap();
        repository.prune_before(&tenant, 1_700_000_000_000, &approval).unwrap();
        let backend = repository.into_backend();
        let statements = backend.statements.lock().unwrap();
        assert_eq!(statements[0].operation, GraphOperation::Append);
        assert_eq!(statements[1].operation, GraphOperation::Delete);
        assert_eq!(statements[1].params["approved_by"], json!("admin-1"));
        assert!(statements[1].cypher.contains("DeletionRun"));
    }

    #[test]
    fn invalid_identifiers_and_feedback_are_rejected_before_backend() {
        let repository = CompanyMemoryRepository::new(FakeBackend::default());
        let tenant = scope("tenant-a");
        let error = repository.recall_session(&tenant, "x' MATCH (n)").unwrap_err();
        assert!(matches!(error, CompanyMemoryError::InvalidInput(_)));
        let error =
            repository.remember_feedback(&tenant, &FeedbackMemory { answer_id: "a-1".into(), feedback_id: "f-1".into(), author_id: "user-1".into(), rating: 2, comment: "bad".into() }).unwrap_err();
        assert!(matches!(error, CompanyMemoryError::InvalidInput(_)));
        assert!(repository.into_backend().statements.lock().unwrap().is_empty());
    }
}
