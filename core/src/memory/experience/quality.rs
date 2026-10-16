//! Quality, confidence, lifecycle and ranking of experiences.
//!
//! Trust is earned from evidence and lost the same way: a new experience starts with low
//! confidence, real verification raises it, successful reuse raises it further, failures lower it
//! faster than successes raise it, and time without re-verification makes it stale.

use super::model::{now_secs, Experience, ExperienceStatus, Verification};
use super::settings::LearningSettings;

const DAY: i64 = 86_400;

/// Confidence of a brand-new experience: low, and only a little higher when the outcome was
/// actually verified. One success never makes a memory trusted.
pub fn initial_confidence(v: &Verification) -> f32 {
    if v.verified {
        (0.30 + 0.30 * v.method.weight()).min(0.60)
    } else {
        0.15
    }
}

/// 0.0 (just verified) counting down to 0.0 when very old; 1.0 = fresh.
pub fn freshness(last_verified_at: i64, now: i64, stale_after_days: u32) -> f32 {
    if last_verified_at <= 0 {
        return 0.0;
    }
    let age_days = ((now - last_verified_at).max(0) as f32) / DAY as f32;
    (1.0 - age_days / (2.0 * stale_after_days.max(1) as f32)).clamp(0.0, 1.0)
}

/// How complete the record is: the fields that make it reusable.
pub fn completeness(e: &Experience) -> f32 {
    let filled = [
        !e.problem_summary.trim().is_empty(),
        !e.approach.trim().is_empty(),
        !e.important_steps.is_empty(),
        !e.tools_used.is_empty(),
        !e.solution_summary.trim().is_empty(),
        !e.verification_result.trim().is_empty(),
    ];
    filled.iter().filter(|f| **f).count() as f32 / filled.len() as f32
}

/// Overall quality 0..1: correctness, verification quality, reuse record, completeness, freshness.
/// An unverified experience cannot score above 0.4 whatever else is true of it.
pub fn quality_score(e: &Experience, now: i64, stale_after_days: u32) -> f32 {
    let correctness = if e.success { 1.0 } else { 0.0 };
    let verification = if e.verified { e.verification_method.weight() } else { 0.0 };
    let done = e.successful_reuse_count + e.failed_reuse_count;
    // No reuse yet is neutral, not bad.
    let reuse = if done == 0 { 0.5 } else { e.reuse_success_rate() };
    let score = 0.25 * correctness
        + 0.30 * verification
        + 0.20 * reuse
        + 0.15 * completeness(e)
        + 0.10 * freshness(e.last_verified_at.max(e.updated_at), now, stale_after_days);
    let score = if e.verified { score } else { score.min(0.4) };
    score.clamp(0.0, 1.0)
}

/// Record the outcome of using an experience: counters, confidence, then lifecycle.
/// Failures cost more confidence than successes add.
pub fn apply_reuse_outcome(e: &mut Experience, success: bool, settings: &LearningSettings) {
    let now = now_secs();
    e.reuse_count += 1;
    e.last_used_at = now;
    if success {
        e.successful_reuse_count += 1;
        e.confidence += (1.0 - e.confidence) * 0.15;
        e.last_verified_at = now;
    } else {
        e.failed_reuse_count += 1;
        e.confidence -= e.confidence * 0.35;
    }
    e.confidence = e.confidence.clamp(0.0, 1.0);
    e.status = next_status(e, settings, now);
    e.quality_score = quality_score(e, now, settings.stale_after_days);
    e.updated_at = now;
}

/// The lifecycle rules in one place.
pub fn next_status(e: &Experience, settings: &LearningSettings, now: i64) -> ExperienceStatus {
    use ExperienceStatus::*;
    // A person's decision on the way down is final until a person or a revalidation changes it.
    if matches!(e.status, Invalid | Failed) {
        return e.status;
    }
    let done = e.successful_reuse_count + e.failed_reuse_count;
    let rate = e.reuse_success_rate();

    // Repeated failure: stop trusting it.
    if e.failed_reuse_count >= 3 && rate < 0.5 {
        return Deprecated;
    }
    if e.status == Deprecated || e.status == Stale {
        // Only revalidation brings these back (it resets the status itself).
        return e.status;
    }
    // Old and unchecked: needs revalidation before it is used again.
    let last_check = e.last_verified_at.max(if e.verified { e.created_at } else { 0 });
    if e.status.is_usable() && last_check > 0 && now - last_check > settings.stale_after_days as i64 * DAY {
        return Stale;
    }
    if e.human_approved && e.verified {
        return if e.reuse_count >= 5 && rate >= 0.8 { FrequentlyReused } else { Trusted };
    }
    if !e.verified {
        return Candidate;
    }
    if settings.require_human_approval && !e.human_approved {
        // Verified evidence is recorded, but only a person promotes past VERIFIED.
        return if e.status == Candidate { Verified } else { e.status.min_rank(Verified) };
    }
    // One failure after real use drops a trusted memory back a step.
    if matches!(e.status, Trusted | FrequentlyReused) && done >= 2 && rate < 0.6 {
        return Verified;
    }
    if e.successful_reuse_count >= 2 && rate >= 0.75 && e.confidence >= 0.6 {
        if e.reuse_count >= 5 && rate >= 0.8 {
            return FrequentlyReused;
        }
        return Trusted;
    }
    match e.status {
        Candidate => Verified,
        other => other,
    }
}

impl ExperienceStatus {
    fn rank(&self) -> u8 {
        match self {
            Self::Candidate => 0,
            Self::Verified => 1,
            Self::Trusted => 2,
            Self::FrequentlyReused => 3,
            _ => 0,
        }
    }
    /// The lower of `self` and `cap` on the CANDIDATE..FREQUENTLY_REUSED ladder.
    fn min_rank(self, cap: ExperienceStatus) -> ExperienceStatus {
        if self.rank() <= cap.rank() { self } else { cap }
    }
}

/// What the current task looks like, for ranking.
#[derive(Debug, Clone, Default)]
pub struct RankContext {
    pub task_type: String,
    pub project_key: String,
    pub technologies: Vec<String>,
}

/// Weights for the ranking score. They sum to 1.0 and can be tuned in one place.
#[derive(Debug, Clone, Copy)]
pub struct RankWeights {
    pub similarity: f32,
    pub task_type: f32,
    pub project: f32,
    pub technology: f32,
    pub history: f32,
    pub verification: f32,
    pub reuse: f32,
    pub confidence: f32,
    pub freshness: f32,
}

impl Default for RankWeights {
    fn default() -> Self {
        Self { similarity: 0.35, task_type: 0.10, project: 0.10, technology: 0.08, history: 0.07, verification: 0.10, reuse: 0.08, confidence: 0.07, freshness: 0.05 }
    }
}

/// Rank one candidate. Semantic similarity is only a third of the score: a close embedding from a
/// different project, an unverified run or a year-old memory does not win on wording alone.
pub fn ranking_score(e: &Experience, similarity: f32, ctx: &RankContext, w: &RankWeights, now: i64, stale_after_days: u32) -> f32 {
    let same = |a: &str, b: &str| !a.is_empty() && a.eq_ignore_ascii_case(b);
    let task_type = if same(&e.task_type, &ctx.task_type) { 1.0 } else { 0.0 };
    let project = if e.project_key.is_empty() {
        0.5 // a general experience: neutral
    } else if same(&e.project_key, &ctx.project_key) {
        1.0
    } else {
        0.0
    };
    let technology = if e.technologies.is_empty() || ctx.technologies.is_empty() {
        0.5
    } else {
        let hit = e.technologies.iter().filter(|t| ctx.technologies.iter().any(|c| c.eq_ignore_ascii_case(t))).count();
        hit as f32 / e.technologies.len() as f32
    };
    let history = if e.success { 1.0 } else { 0.0 };
    let verification = if e.verified { e.verification_method.weight() } else { 0.0 };
    let done = e.successful_reuse_count + e.failed_reuse_count;
    let reuse = if done == 0 { 0.5 } else { e.reuse_success_rate() };
    let fresh = freshness(e.last_verified_at.max(e.updated_at), now, stale_after_days);
    let score = w.similarity * similarity.clamp(0.0, 1.0)
        + w.task_type * task_type
        + w.project * project
        + w.technology * technology
        + w.history * history
        + w.verification * verification
        + w.reuse * reuse
        + w.confidence * e.confidence
        + w.freshness * fresh;
    score.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::experience::model::{ExperienceStatus as S, VerificationMethod as M};

    fn settings() -> LearningSettings {
        LearningSettings::default()
    }

    fn verified_experience() -> Experience {
        let mut e = Experience::candidate("read a market page", "live_data");
        e.approach = "search then read snapshot".into();
        e.important_steps = vec!["navigate".into(), "snapshot".into()];
        e.tools_used = vec!["browser_navigate".into()];
        e.solution_summary = "quote figures from the page".into();
        e.verification_result = "figures found on page".into();
        e.verification_method = M::PageEvidence;
        e.verified = true;
        e.success = true;
        e.last_verified_at = now_secs();
        e.confidence = initial_confidence(&Verification::new(true, M::PageEvidence, "ok"));
        e.status = S::Verified;
        e
    }

    #[test]
    fn a_new_experience_starts_with_low_confidence_and_evidence_raises_it() {
        let claim = initial_confidence(&Verification::new(true, M::ModelClaim, "done"));
        let page = initial_confidence(&Verification::new(true, M::PageEvidence, "ok"));
        let tests = initial_confidence(&Verification::new(true, M::Tests, "passed"));
        assert!(claim < 0.2 && claim < page && page < tests && tests <= 0.6, "{claim} {page} {tests}");
    }

    #[test]
    fn one_success_does_not_make_a_memory_trusted_but_repeated_success_does() {
        let mut e = verified_experience();
        apply_reuse_outcome(&mut e, true, &settings());
        assert_eq!(e.status, S::Verified, "one reuse is not enough");
        for _ in 0..3 {
            apply_reuse_outcome(&mut e, true, &settings());
        }
        assert!(matches!(e.status, S::Trusted | S::FrequentlyReused), "{:?} conf {}", e.status, e.confidence);
        assert!(e.confidence > 0.6);
        for _ in 0..3 {
            apply_reuse_outcome(&mut e, true, &settings());
        }
        assert_eq!(e.status, S::FrequentlyReused);
    }

    #[test]
    fn failures_cost_more_than_successes_and_repeated_failure_deprecates() {
        let mut e = verified_experience();
        e.confidence = 0.6;
        let mut win = e.clone();
        apply_reuse_outcome(&mut win, true, &settings());
        let mut lose = e.clone();
        apply_reuse_outcome(&mut lose, false, &settings());
        assert!(0.6 - lose.confidence > win.confidence - 0.6, "failure must hurt more");
        for _ in 0..3 {
            apply_reuse_outcome(&mut e, false, &settings());
        }
        assert_eq!(e.status, S::Deprecated);
        // A deprecated memory does not recover just because a later reuse succeeded.
        apply_reuse_outcome(&mut e, true, &settings());
        assert_eq!(e.status, S::Deprecated);
    }

    #[test]
    fn an_unverified_run_never_becomes_verified_and_scores_low() {
        let mut e = Experience::candidate("x", "t");
        e.success = true;
        e.approach = "a".into();
        e.important_steps = vec!["s".into()];
        e.tools_used = vec!["t".into()];
        e.solution_summary = "s".into();
        assert_eq!(next_status(&e, &settings(), now_secs()), S::Candidate);
        assert!(quality_score(&e, now_secs(), 30) <= 0.4);
        e.verified = true;
        e.verification_method = M::Tests;
        assert!(quality_score(&e, now_secs(), 30) > 0.5);
    }

    #[test]
    fn old_experiences_go_stale_and_human_approval_is_respected() {
        let mut e = verified_experience();
        let long_ago = now_secs() - 90 * DAY;
        e.last_verified_at = long_ago;
        e.created_at = long_ago;
        assert_eq!(next_status(&e, &settings(), now_secs()), S::Stale);

        let mut strict = settings();
        strict.require_human_approval = true;
        let mut e = verified_experience();
        for _ in 0..5 {
            apply_reuse_outcome(&mut e, true, &strict);
        }
        assert_eq!(e.status, S::Verified, "without a person's approval it stays VERIFIED");
        e.human_approved = true;
        apply_reuse_outcome(&mut e, true, &strict);
        assert!(matches!(e.status, S::Trusted | S::FrequentlyReused));
    }

    #[test]
    fn ranking_prefers_the_right_project_and_evidence_over_wording_alone() {
        let now = now_secs();
        let ctx = RankContext { task_type: "live_data".into(), project_key: "web:google.com".into(), technologies: vec!["browser".into()] };
        let w = RankWeights::default();
        let mut good = verified_experience();
        good.project_key = "web:google.com".into();
        good.technologies = vec!["browser".into()];
        good.confidence = 0.7;
        let mut wrong_project = good.clone();
        wrong_project.project_key = "web:other.example".into();
        let mut unverified = good.clone();
        unverified.verified = false;
        unverified.verification_method = M::None;
        let g = ranking_score(&good, 0.7, &ctx, &w, now, 30);
        let p = ranking_score(&wrong_project, 0.9, &ctx, &w, now, 30);
        let u = ranking_score(&unverified, 0.9, &ctx, &w, now, 30);
        assert!(g > p, "same project beats a closer embedding from another: {g} vs {p}");
        assert!(g > u, "verified beats unverified: {g} vs {u}");
        let sum = w.similarity + w.task_type + w.project + w.technology + w.history + w.verification + w.reuse + w.confidence + w.freshness;
        assert!((sum - 1.0).abs() < 1e-6);
    }
}
