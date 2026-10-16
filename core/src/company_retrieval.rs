//! Deterministic retrieval planning and evidence quality controls for Company Brain.
//!
//! This module deliberately does not execute graph queries. It selects one of a bounded set of
//! graph templates, ranks normalized evidence returned by the provider, and checks proposed claims
//! against that evidence. The model never supplies Cypher or ranking weights.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

const DEFAULT_LIMIT: usize = 8;
const MIN_CONFIDENCE: f32 = 0.52;
const AMBIGUITY_MARGIN: f32 = 0.035;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    Ownership,
    Rationale,
    Expertise,
    Dependency,
    Incident,
    General,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryTemplate {
    OwnershipPath,
    DecisionRationale,
    ExpertisePath,
    DependencyPath,
    IncidentContext,
    ConnectedContext,
}

impl QueryTemplate {
    /// Stable identifier used by the provider to select a hard-coded, parameterized query.
    pub fn id(self) -> &'static str {
        match self {
            Self::OwnershipPath => "ownership_path_v1",
            Self::DecisionRationale => "decision_rationale_v1",
            Self::ExpertisePath => "expertise_path_v1",
            Self::DependencyPath => "dependency_path_v1",
            Self::IncidentContext => "incident_context_v1",
            Self::ConnectedContext => "connected_context_v1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Ticket,
    Incident,
    Decision,
    Project,
    Person,
    Team,
    System,
    Topic,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryEntity {
    pub kind: EntityKind,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalPlan {
    pub intent: QueryIntent,
    pub template: QueryTemplate,
    pub entities: Vec<QueryEntity>,
    pub search_terms: Vec<String>,
    pub limit: usize,
    pub needs_clarification: bool,
    pub ambiguity_reasons: Vec<String>,
}

/// Create a safe plan. `template` is an enum, so callers cannot introduce arbitrary Cypher.
pub fn plan(question: &str) -> RetrievalPlan {
    let normalized = question.trim();
    let lower = normalized.to_ascii_lowercase();
    let intent = classify_intent(&lower);
    let template = match intent {
        QueryIntent::Ownership => QueryTemplate::OwnershipPath,
        QueryIntent::Rationale => QueryTemplate::DecisionRationale,
        QueryIntent::Expertise => QueryTemplate::ExpertisePath,
        QueryIntent::Dependency => QueryTemplate::DependencyPath,
        QueryIntent::Incident => QueryTemplate::IncidentContext,
        QueryIntent::General => QueryTemplate::ConnectedContext,
    };
    let entities = extract_entities(normalized);
    let search_terms = meaningful_terms(normalized);
    let mut ambiguity_reasons = Vec::new();
    if normalized.len() < 3 {
        ambiguity_reasons.push("the question is too short to identify company context".into());
    }
    let needs_subject = matches!(intent, QueryIntent::Ownership | QueryIntent::Rationale | QueryIntent::Expertise | QueryIntent::Dependency | QueryIntent::Incident);
    if needs_subject && entities.is_empty() && search_terms.len() < 2 {
        ambiguity_reasons.push(format!("the {:?} question does not identify a subject", intent).to_ascii_lowercase());
    }
    RetrievalPlan { intent, template, entities, search_terms, limit: DEFAULT_LIMIT, needs_clarification: !ambiguity_reasons.is_empty(), ambiguity_reasons }
}

fn classify_intent(lower: &str) -> QueryIntent {
    if contains_any(lower, &["who owns", "owner", "responsible", "accountable", "maintainer"]) {
        QueryIntent::Ownership
    } else if contains_any(lower, &["why", "rationale", "reason", "decision", "decided", "chosen"]) {
        QueryIntent::Rationale
    } else if contains_any(lower, &["expert", "expertise", "knows about", "experienced", "subject matter"]) {
        QueryIntent::Expertise
    } else if contains_any(lower, &["depend", "blocked by", "upstream", "downstream", "impact", "relies on"]) {
        QueryIntent::Dependency
    } else if contains_any(lower, &["incident", "outage", "sev", "root cause", "postmortem"]) {
        QueryIntent::Incident
    } else {
        QueryIntent::General
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn extract_entities(question: &str) -> Vec<QueryEntity> {
    let mut found = BTreeSet::new();
    let bytes = question.as_bytes();
    let mut quote_start = None;
    for (idx, byte) in bytes.iter().enumerate() {
        if matches!(*byte, b'\'' | b'"') {
            if let Some(start) = quote_start.take() {
                let value = question[start..idx].trim();
                if !value.is_empty() {
                    found.insert((EntityKind::Topic, value.to_string()));
                }
            } else {
                quote_start = Some(idx + 1);
            }
        }
    }

    for raw in question.split(|c: char| c.is_whitespace() || matches!(c, ',' | '?' | '(' | ')' | ':' | ';')) {
        let token = raw.trim_matches(|c: char| matches!(c, '.' | '!' | '\'' | '"'));
        let upper = token.to_ascii_uppercase();
        let kind = if id_with_prefix(&upper, &["INC-", "SEV-"]) {
            Some(EntityKind::Incident)
        } else if id_with_prefix(&upper, &["TICKET-", "TASK-", "BUG-"]) {
            Some(EntityKind::Ticket)
        } else if id_with_prefix(&upper, &["DEC-", "ADR-"]) {
            Some(EntityKind::Decision)
        } else if id_with_prefix(&upper, &["PROJ-", "PROJECT-"]) {
            Some(EntityKind::Project)
        } else {
            None
        };
        if let Some(kind) = kind {
            found.insert((kind, token.to_string()));
        }
    }
    found.into_iter().map(|(kind, value)| QueryEntity { kind, value }).collect()
}

fn id_with_prefix(value: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| value.strip_prefix(prefix).is_some_and(|tail| !tail.is_empty() && tail.len() <= 64 && tail.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')))
}

fn meaningful_terms(text: &str) -> Vec<String> {
    const STOP: &[&str] =
        &["about", "are", "can", "does", "for", "from", "has", "have", "how", "into", "our", "should", "that", "the", "their", "this", "was", "what", "when", "where", "which", "who", "why", "with"];
    let mut terms = BTreeSet::new();
    for word in text.to_ascii_lowercase().split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_') {
        if word.len() >= 3 && !STOP.contains(&word) {
            terms.insert(word.to_string());
        }
    }
    terms.into_iter().collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceClaim {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub id: String,
    pub title: String,
    pub text: String,
    pub source_id: String,
    pub source_uri: String,
    pub path: Vec<String>,
    /// Unix timestamp in seconds. Zero means freshness is unknown.
    pub updated_at: i64,
    /// Source authority from 0 (unknown) through 100 (canonical policy/record).
    pub authority: u8,
    pub claims: Vec<EvidenceClaim>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RankedEvidence {
    pub evidence: EvidenceRecord,
    pub score: f32,
    pub term_match: f32,
    pub freshness: f32,
    pub authority: f32,
    pub path_quality: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalStatus {
    Sufficient,
    Insufficient,
    Ambiguous,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetrievalResult {
    pub status: RetrievalStatus,
    pub confidence: f32,
    pub evidence: Vec<RankedEvidence>,
    pub ambiguity_reasons: Vec<String>,
}

/// Rank evidence using fixed weights: match 45%, authority 25%, freshness 20%, path 10%.
pub fn rank(plan: &RetrievalPlan, records: Vec<EvidenceRecord>, now_unix: i64) -> RetrievalResult {
    let mut ranked: Vec<_> = records.into_iter().map(|evidence| score_record(plan, evidence, now_unix)).collect();
    ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal).then_with(|| a.evidence.id.cmp(&b.evidence.id)));
    ranked.truncate(plan.limit);

    let confidence = ranked.first().map(|item| item.score).unwrap_or(0.0);
    let mut ambiguity_reasons = plan.ambiguity_reasons.clone();
    if ranked.len() > 1 && (ranked[0].score - ranked[1].score).abs() <= AMBIGUITY_MARGIN && conflicting_top_claims(&ranked[0].evidence, &ranked[1].evidence) {
        ambiguity_reasons.push("the two strongest sources make conflicting claims with similar support".into());
    }
    let status = if !ambiguity_reasons.is_empty() {
        RetrievalStatus::Ambiguous
    } else if confidence < MIN_CONFIDENCE {
        RetrievalStatus::Insufficient
    } else {
        RetrievalStatus::Sufficient
    };
    RetrievalResult { status, confidence, evidence: ranked, ambiguity_reasons }
}

fn score_record(plan: &RetrievalPlan, evidence: EvidenceRecord, now_unix: i64) -> RankedEvidence {
    let mut words = meaningful_terms(&format!("{} {} {} {}", evidence.id, evidence.title, evidence.text, evidence.path.join(" ")));
    for claim in &evidence.claims {
        words.extend(meaningful_terms(&format!("{} {} {}", claim.subject, claim.predicate, claim.object)));
    }
    words.sort();
    words.dedup();
    let query: BTreeSet<&str> = plan.search_terms.iter().map(String::as_str).collect();
    let matched = words.iter().filter(|word| query.contains(word.as_str())).count();
    let term_match = if query.is_empty() { 0.5 } else { (matched as f32 / query.len() as f32).min(1.0) };
    let authority = (evidence.authority.min(100) as f32) / 100.0;
    let freshness = freshness_score(evidence.updated_at, now_unix);
    let hops = evidence.path.len().saturating_sub(1) / 2;
    let path_quality = if evidence.path.is_empty() { 0.0 } else { (1.0 - (hops.saturating_sub(1) as f32 * 0.15)).max(0.25) };
    let score = term_match * 0.45 + authority * 0.25 + freshness * 0.20 + path_quality * 0.10;
    RankedEvidence { evidence, score, term_match, freshness, authority, path_quality }
}

fn freshness_score(updated_at: i64, now_unix: i64) -> f32 {
    if updated_at <= 0 || now_unix <= 0 {
        return 0.5;
    }
    let age_days = now_unix.saturating_sub(updated_at).max(0) as f32 / 86_400.0;
    match age_days {
        age if age <= 7.0 => 1.0,
        age if age <= 30.0 => 0.85,
        age if age <= 90.0 => 0.65,
        age if age <= 365.0 => 0.4,
        _ => 0.2,
    }
}

fn conflicting_top_claims(a: &EvidenceRecord, b: &EvidenceRecord) -> bool {
    a.claims.iter().any(|left| {
        b.claims.iter().any(|right| {
            normalized(&left.subject) == normalized(&right.subject) && normalized(&left.predicate) == normalized(&right.predicate) && normalized(&left.object) != normalized(&right.object)
        })
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimVerdict {
    Supported,
    Contradicted,
    Insufficient,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimCheck {
    pub claim: EvidenceClaim,
    pub verdict: ClaimVerdict,
    pub supporting_source_ids: Vec<String>,
    pub contradicting_source_ids: Vec<String>,
}

/// Require every answer claim to match a retrieved graph claim; string matching is normalized but exact.
pub fn validate_claims(claims: &[EvidenceClaim], evidence: &[RankedEvidence]) -> Vec<ClaimCheck> {
    claims
        .iter()
        .map(|claim| {
            let mut supporting = BTreeSet::new();
            let mut contradicting = BTreeSet::new();
            for item in evidence {
                for known in &item.evidence.claims {
                    if normalized(&known.subject) == normalized(&claim.subject) && normalized(&known.predicate) == normalized(&claim.predicate) {
                        if normalized(&known.object) == normalized(&claim.object) {
                            supporting.insert(item.evidence.source_id.clone());
                        } else {
                            contradicting.insert(item.evidence.source_id.clone());
                        }
                    }
                }
            }
            let verdict = if !supporting.is_empty() {
                ClaimVerdict::Supported
            } else if !contradicting.is_empty() {
                ClaimVerdict::Contradicted
            } else {
                ClaimVerdict::Insufficient
            };
            ClaimCheck { claim: claim.clone(), verdict, supporting_source_ids: supporting.into_iter().collect(), contradicting_source_ids: contradicting.into_iter().collect() }
        })
        .collect()
}

/// A concise reason suitable for blocking an ungrounded answer or asking the user to clarify.
pub fn refusal_reason(result: &RetrievalResult) -> Option<String> {
    match result.status {
        RetrievalStatus::Sufficient => None,
        RetrievalStatus::Ambiguous => Some(result.ambiguity_reasons.first().cloned().unwrap_or_else(|| "the company graph contains ambiguous evidence".into())),
        RetrievalStatus::Insufficient => Some(format!("connected evidence confidence {:.2} is below the {:.2} threshold", result.confidence, MIN_CONFIDENCE)),
    }
}

fn normalized(value: &str) -> String {
    value.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Summarize evidence per source without exposing internal scoring implementation details.
pub fn sources(result: &RetrievalResult) -> BTreeMap<String, String> {
    result.evidence.iter().map(|item| (item.evidence.source_id.clone(), item.evidence.source_uri.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn claim(subject: &str, predicate: &str, object: &str) -> EvidenceClaim {
        EvidenceClaim { subject: subject.into(), predicate: predicate.into(), object: object.into() }
    }

    fn evidence(id: &str, text: &str, source: &str, authority: u8, age_days: i64, path: &[&str], claims: Vec<EvidenceClaim>) -> EvidenceRecord {
        EvidenceRecord {
            id: id.into(),
            title: text.into(),
            text: text.into(),
            source_id: source.into(),
            source_uri: format!("company://{source}"),
            path: path.iter().map(|v| (*v).into()).collect(),
            updated_at: NOW - age_days * 86_400,
            authority,
            claims,
        }
    }

    #[test]
    fn ownership_selects_a_bounded_template_and_ranks_the_canonical_owner() {
        let planned = plan("Who owns INC-1042?");
        assert_eq!((planned.intent, planned.template, planned.template.id()), (QueryIntent::Ownership, QueryTemplate::OwnershipPath, "ownership_path_v1"));
        assert_eq!(planned.entities[0], QueryEntity { kind: EntityKind::Incident, value: "INC-1042".into() });
        let result = rank(
            &planned,
            vec![
                evidence("old", "INC-1042 owner Priya", "chat-2", 35, 300, &["INC-1042", "OWNED_BY", "Priya"], vec![claim("INC-1042", "owned_by", "Priya")]),
                evidence(
                    "canonical",
                    "INC-1042 owner Identity Team",
                    "catalog-1",
                    100,
                    2,
                    &["INC-1042", "BELONGS_TO", "Identity", "OWNED_BY", "Identity Team"],
                    vec![claim("INC-1042", "owned_by", "Identity Team")],
                ),
            ],
            NOW,
        );
        assert_eq!(result.status, RetrievalStatus::Sufficient);
        assert_eq!(result.evidence[0].evidence.id, "canonical");
    }

    #[test]
    fn every_supported_intent_maps_to_one_closed_query_template() {
        let cases = [
            ("Who owns PROJ-7?", QueryIntent::Ownership, QueryTemplate::OwnershipPath),
            ("Why was DEC-14 chosen?", QueryIntent::Rationale, QueryTemplate::DecisionRationale),
            ("Who has expertise in 'FalkorDB'?", QueryIntent::Expertise, QueryTemplate::ExpertisePath),
            ("What does 'Checkout' depend on?", QueryIntent::Dependency, QueryTemplate::DependencyPath),
            ("Show the incident context for INC-9", QueryIntent::Incident, QueryTemplate::IncidentContext),
            ("Summarize connections around 'Identity'", QueryIntent::General, QueryTemplate::ConnectedContext),
        ];
        let mut ids = BTreeSet::new();
        for (question, intent, template) in cases {
            let planned = plan(question);
            assert_eq!((planned.intent, planned.template), (intent, template));
            assert!(ids.insert(planned.template.id()));
            assert!(planned.limit <= DEFAULT_LIMIT);
        }
    }

    #[test]
    fn rationale_prefers_fresh_authoritative_decision_evidence() {
        let planned = plan("Why was DEC-014 chosen for OAuth?");
        assert_eq!(planned.template, QueryTemplate::DecisionRationale);
        let result = rank(
            &planned,
            vec![
                evidence("chat", "DEC-014 OAuth guess", "chat", 20, 1, &["DEC-014", "MENTIONED_IN", "chat"], vec![]),
                evidence(
                    "adr",
                    "DEC-014 OAuth staged migration rationale chosen",
                    "adr-14",
                    95,
                    10,
                    &["DEC-014", "SUPPORTED_BY", "ADR-014"],
                    vec![claim("DEC-014", "rationale", "reduce migration risk")],
                ),
            ],
            NOW,
        );
        assert_eq!(result.evidence[0].evidence.id, "adr");
        assert!(result.confidence >= MIN_CONFIDENCE);
    }

    #[test]
    fn expertise_requires_a_subject_and_validates_the_supported_claim() {
        let ambiguous = plan("Who is the expert?");
        assert!(ambiguous.needs_clarification);
        let planned = plan("Who has expertise in 'FalkorDB performance'?");
        assert_eq!(planned.template, QueryTemplate::ExpertisePath);
        assert!(!planned.needs_clarification);
        let result = rank(
            &planned,
            vec![evidence(
                "expert",
                "FalkorDB performance expert Maya",
                "skills-1",
                90,
                3,
                &["Maya", "HAS_EXPERTISE", "FalkorDB performance"],
                vec![claim("Maya", "has_expertise", "FalkorDB performance")],
            )],
            NOW,
        );
        let checks = validate_claims(&[claim("Maya", "has_expertise", "FalkorDB performance")], &result.evidence);
        assert_eq!(checks[0].verdict, ClaimVerdict::Supported);
        assert_eq!(checks[0].supporting_source_ids, vec!["skills-1"]);
    }

    #[test]
    fn dependency_conflicts_are_reported_as_ambiguous() {
        let planned = plan("What does 'Checkout' depend on?");
        assert_eq!(planned.template, QueryTemplate::DependencyPath);
        let records = vec![
            evidence("a", "Checkout depends payments", "arch-a", 90, 5, &["Checkout", "DEPENDS_ON", "Payments"], vec![claim("Checkout", "depends_on", "Payments")]),
            evidence("b", "Checkout depends billing", "arch-b", 90, 5, &["Checkout", "DEPENDS_ON", "Billing"], vec![claim("Checkout", "depends_on", "Billing")]),
        ];
        let result = rank(&planned, records, NOW);
        assert_eq!(result.status, RetrievalStatus::Ambiguous);
        assert!(refusal_reason(&result).unwrap().contains("conflicting"));
    }

    #[test]
    fn incident_rejects_an_unsupported_root_cause_claim() {
        let planned = plan("What was the root cause of INC-99?");
        assert_eq!(planned.template, QueryTemplate::IncidentContext);
        let result = rank(
            &planned,
            vec![evidence("postmortem", "INC-99 root cause expired certificate", "pm-99", 100, 2, &["INC-99", "DOCUMENTED_BY", "PM-99"], vec![claim("INC-99", "root_cause", "expired certificate")])],
            NOW,
        );
        let checks =
            validate_claims(&[claim("INC-99", "root_cause", "expired certificate"), claim("INC-99", "root_cause", "database overload"), claim("INC-99", "duration", "42 minutes")], &result.evidence);
        assert_eq!(checks.iter().map(|c| c.verdict).collect::<Vec<_>>(), vec![ClaimVerdict::Supported, ClaimVerdict::Contradicted, ClaimVerdict::Insufficient]);
        assert_eq!(sources(&result).get("pm-99").map(String::as_str), Some("company://pm-99"));
    }

    #[test]
    fn weak_evidence_falls_below_the_confidence_threshold() {
        let planned = plan("Tell me about 'Project Atlas'");
        let result = rank(&planned, vec![evidence("noise", "unrelated lunch notes", "chat", 5, 800, &[], vec![])], NOW);
        assert_eq!(result.status, RetrievalStatus::Insufficient);
        assert!(refusal_reason(&result).unwrap().contains("below"));
    }
}
