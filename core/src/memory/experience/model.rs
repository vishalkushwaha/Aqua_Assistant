//! The Agent Experience record and its enums.
//!
//! An experience is a *reusable, verified way of solving a kind of task*: the problem, the approach,
//! the steps and tools, how it was verified and what happened when it was reused. It is deliberately
//! not a transcript and never holds the model's private reasoning.

use serde::{Deserialize, Serialize};

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $text:expr),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $( #[serde(rename = $text)] $variant ),+ }
        impl $name {
            pub fn as_str(&self) -> &'static str { match self { $( Self::$variant => $text ),+ } }
            pub fn parse(s: &str) -> Option<Self> {
                match s.trim().to_ascii_uppercase().as_str() { $( t if t == $text => Some(Self::$variant), )+ _ => None }
            }
        }
    };
}

string_enum!(
    /// What kind of knowledge this is.
    ExperienceType {
        Solution => "SOLUTION",
        Procedure => "PROCEDURE",
        Pattern => "PATTERN",
        ErrorFix => "ERROR_FIX",
        ProjectKnowledge => "PROJECT_KNOWLEDGE",
        Constraint => "CONSTRAINT",
        BestPractice => "BEST_PRACTICE",
    }
);

string_enum!(
    /// Lifecycle. CANDIDATE -> VERIFIED -> TRUSTED -> FREQUENTLY_REUSED, or down to
    /// FAILED / STALE / DEPRECATED / INVALID. Only the first four influence the agent.
    ExperienceStatus {
        Candidate => "CANDIDATE",
        Verified => "VERIFIED",
        Trusted => "TRUSTED",
        FrequentlyReused => "FREQUENTLY_REUSED",
        Failed => "FAILED",
        Stale => "STALE",
        Deprecated => "DEPRECATED",
        Invalid => "INVALID",
    }
);

string_enum!(
    /// Who may see it. Aqua is a single-user local app, so this is GLOBAL (any task),
    /// PROJECT (one site or workspace) or USER (this user's own habits).
    ExperienceScope {
        Global => "GLOBAL",
        Project => "PROJECT",
        User => "USER",
    }
);

impl ExperienceStatus {
    /// Whether the agent may be shown an experience in this state.
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Verified | Self::Trusted | Self::FrequentlyReused)
    }
    pub fn is_retired(&self) -> bool {
        matches!(self, Self::Failed | Self::Stale | Self::Deprecated | Self::Invalid)
    }
}

/// How an outcome was checked. Stronger evidence counts for more; the model saying "done" is not
/// evidence at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMethod {
    Tests,
    Build,
    TypeCheck,
    Lint,
    /// Figures or facts in the answer were found in what the agent actually read.
    PageEvidence,
    /// A tool call succeeded and its result covers the goal.
    ToolResult,
    /// Compared with an expected output.
    ExpectedOutput,
    /// A person confirmed it.
    Human,
    /// The model claims success. Never counts as verification.
    ModelClaim,
    None,
}

impl VerificationMethod {
    /// 0.0 (worthless) to 1.0 (as strong as it gets).
    pub fn weight(&self) -> f32 {
        match self {
            Self::Human => 1.0,
            Self::Tests | Self::Build => 1.0,
            Self::TypeCheck | Self::ExpectedOutput => 0.85,
            Self::Lint => 0.6,
            Self::PageEvidence => 0.75,
            Self::ToolResult => 0.6,
            Self::ModelClaim => 0.05,
            Self::None => 0.0,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tests => "tests",
            Self::Build => "build",
            Self::TypeCheck => "type_check",
            Self::Lint => "lint",
            Self::PageEvidence => "page_evidence",
            Self::ToolResult => "tool_result",
            Self::ExpectedOutput => "expected_output",
            Self::Human => "human",
            Self::ModelClaim => "model_claim",
            Self::None => "none",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "tests" => Self::Tests,
            "build" => Self::Build,
            "type_check" => Self::TypeCheck,
            "lint" => Self::Lint,
            "page_evidence" => Self::PageEvidence,
            "tool_result" => Self::ToolResult,
            "expected_output" => Self::ExpectedOutput,
            "human" => Self::Human,
            "model_claim" => Self::ModelClaim,
            _ => Self::None,
        }
    }
}

/// Structured verification outcome (`verified`, method, result, timestamp).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    pub verified: bool,
    pub method: VerificationMethod,
    pub result: String,
    pub timestamp: i64,
}

impl Verification {
    pub fn none() -> Self {
        Self { verified: false, method: VerificationMethod::None, result: String::new(), timestamp: now_secs() }
    }
    /// Only real evidence verifies: a model's own claim never does.
    pub fn new(passed: bool, method: VerificationMethod, result: impl Into<String>) -> Self {
        Self { verified: passed && method.weight() >= 0.5, method, result: result.into(), timestamp: now_secs() }
    }
}

/// One stored experience. Field names follow Aqua's snake_case JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Experience {
    pub id: i64,
    pub scope: ExperienceScope,
    /// For PROJECT scope: the site or workspace this belongs to (for example `web:google.com`).
    pub project_key: String,
    pub owner_id: String,
    pub task_type: String,
    pub experience_type: ExperienceType,
    pub problem_summary: String,
    pub context_summary: String,
    pub approach: String,
    pub important_steps: Vec<String>,
    pub tools_used: Vec<String>,
    pub constraints: Vec<String>,
    pub technologies: Vec<String>,
    pub solution_summary: String,
    pub verification_method: VerificationMethod,
    pub verification_result: String,
    pub verified: bool,
    pub result: String,
    pub success: bool,
    /// Learned from a failure that was then corrected (all three set together).
    pub failed_approach: Option<String>,
    pub failure_reason: Option<String>,
    pub correction: Option<String>,
    pub confidence: f32,
    pub quality_score: f32,
    pub status: ExperienceStatus,
    pub human_approved: bool,
    pub embedding_model: String,
    pub embedding_version: u32,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_verified_at: i64,
    pub last_used_at: i64,
    pub retrieval_count: u32,
    pub reuse_count: u32,
    pub successful_reuse_count: u32,
    pub failed_reuse_count: u32,
    pub source_task_id: String,
    pub project_version: String,
    pub experience_version: u32,
}

impl Experience {
    /// A fresh CANDIDATE with everything unknown left empty. Callers fill in what they know.
    pub fn candidate(problem: impl Into<String>, task_type: impl Into<String>) -> Self {
        let t = now_secs();
        Self {
            id: 0,
            scope: ExperienceScope::Global,
            project_key: String::new(),
            owner_id: String::new(),
            task_type: task_type.into(),
            experience_type: ExperienceType::Solution,
            problem_summary: problem.into(),
            context_summary: String::new(),
            approach: String::new(),
            important_steps: Vec::new(),
            tools_used: Vec::new(),
            constraints: Vec::new(),
            technologies: Vec::new(),
            solution_summary: String::new(),
            verification_method: VerificationMethod::None,
            verification_result: String::new(),
            verified: false,
            result: String::new(),
            success: false,
            failed_approach: None,
            failure_reason: None,
            correction: None,
            confidence: 0.2,
            quality_score: 0.0,
            status: ExperienceStatus::Candidate,
            human_approved: false,
            embedding_model: String::new(),
            embedding_version: 0,
            created_at: t,
            updated_at: t,
            last_verified_at: 0,
            last_used_at: 0,
            retrieval_count: 0,
            reuse_count: 0,
            successful_reuse_count: 0,
            failed_reuse_count: 0,
            source_task_id: String::new(),
            project_version: String::new(),
            experience_version: 1,
        }
    }

    pub fn reuse_success_rate(&self) -> f32 {
        let done = self.successful_reuse_count + self.failed_reuse_count;
        if done == 0 {
            0.0
        } else {
            self.successful_reuse_count as f32 / done as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip_through_their_text() {
        for s in [ExperienceStatus::Candidate, ExperienceStatus::FrequentlyReused, ExperienceStatus::Invalid] {
            assert_eq!(ExperienceStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(ExperienceType::parse("error_fix"), Some(ExperienceType::ErrorFix));
        assert_eq!(ExperienceScope::parse("nonsense"), None);
        assert!(ExperienceStatus::Trusted.is_usable() && !ExperienceStatus::Candidate.is_usable());
        assert!(ExperienceStatus::Deprecated.is_retired() && !ExperienceStatus::Verified.is_retired());
    }

    #[test]
    fn a_model_claim_never_verifies_but_real_evidence_does() {
        assert!(!Verification::new(true, VerificationMethod::ModelClaim, "I did it").verified);
        assert!(!Verification::new(true, VerificationMethod::None, "").verified);
        assert!(!Verification::new(false, VerificationMethod::Tests, "2 failed").verified);
        assert!(Verification::new(true, VerificationMethod::Tests, "all passed").verified);
        assert!(Verification::new(true, VerificationMethod::PageEvidence, "figures on page").verified);
    }

    #[test]
    fn json_uses_the_documented_names() {
        let e = Experience::candidate("how to read a market page", "live_data");
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["status"], "CANDIDATE");
        assert_eq!(json["experience_type"], "SOLUTION");
        assert_eq!(json["scope"], "GLOBAL");
        let back: Experience = serde_json::from_value(json).unwrap();
        assert_eq!(back, e);
    }
}
