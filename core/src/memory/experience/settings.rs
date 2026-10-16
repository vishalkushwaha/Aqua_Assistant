//! Learning settings, persisted in the experience database (never only in the UI).
//!
//! Four separate switches, so using what is known, learning from finished tasks and experimenting
//! on their own can each be turned on or off without touching the others.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningSettings {
    /// Master switch. Off: no learning jobs run and nothing new is learned.
    pub learning_enabled: bool,
    /// Whether stored experiences are offered to the agent for new tasks.
    pub retrieval_enabled: bool,
    /// Whether finished tasks are evaluated and stored automatically.
    pub auto_memory_creation: bool,
    /// Bounded retry-and-improve loops on their own. Off by default and never needed for learning.
    pub autonomous_experimentation: bool,
    /// Minimum semantic similarity for a candidate to be considered (0.0 - 1.0).
    pub similarity_threshold: f32,
    /// Minimum confidence for an experience to influence a task (0.0 - 1.0).
    pub memory_confidence_threshold: f32,
    /// Top K candidates pulled from the index, and the most that reach the prompt.
    pub retrieval_candidates: u32,
    pub max_experiences_in_prompt: u32,
    /// Hard limits for autonomous experimentation.
    pub max_attempts: u32,
    pub max_execution_secs: u32,
    pub token_budget: u32,
    pub cost_budget_usd: f32,
    pub max_tool_calls: u32,
    pub max_background_jobs: u32,
    /// New experiences are stored as VERIFIED and can be offered. Approval only promotes them to TRUSTED.
    pub require_human_approval: bool,
    /// Experiences unverified for this long are marked STALE and must be revalidated.
    pub stale_after_days: u32,
    /// Practice on its own: while idle, generate a few read-only tasks in the kinds you already use,
    /// run them and learn from them. Off by default; needs `learning_enabled` too.
    pub auto_practice_enabled: bool,
    /// At most this many practice tasks in 24 hours.
    pub practice_max_per_day: u32,
    /// At least this long between two practice tasks.
    pub practice_min_interval_mins: u32,
    /// Practice starts only after nobody has used the agent for this long.
    pub practice_idle_mins: u32,
    /// Planning rounds allowed per practice task (the time limit is `max_execution_secs`).
    pub practice_max_steps: u32,
}

impl Default for LearningSettings {
    fn default() -> Self {
        Self {
            learning_enabled: true,
            retrieval_enabled: true,
            auto_memory_creation: true,
            autonomous_experimentation: false,
            // Tuned for the built-in hash embedding: paraphrases score ~0.4, unrelated tasks < 0.11.
            // A real embedding model would use a higher value.
            similarity_threshold: 0.35,
            memory_confidence_threshold: 0.35,
            retrieval_candidates: 8,
            max_experiences_in_prompt: 2,
            max_attempts: 3,
            max_execution_secs: 300,
            token_budget: 20_000,
            cost_budget_usd: 0.0,
            max_tool_calls: 24,
            max_background_jobs: 4,
            require_human_approval: false,
            stale_after_days: 30,
            auto_practice_enabled: false,
            practice_max_per_day: 3,
            practice_min_interval_mins: 60,
            practice_idle_mins: 10,
            practice_max_steps: 6,
        }
    }
}

impl LearningSettings {
    /// Pull every value into its allowed range. Limits can be tightened from the UI but a value
    /// that would remove the bound altogether (zero attempts as "unlimited", a year-long timeout)
    /// is not accepted.
    pub fn clamped(mut self) -> Self {
        let unit = |v: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { lo };
        self.similarity_threshold = unit(self.similarity_threshold, 0.05, 0.99);
        self.memory_confidence_threshold = unit(self.memory_confidence_threshold, 0.0, 0.99);
        self.retrieval_candidates = self.retrieval_candidates.clamp(1, 50);
        self.max_experiences_in_prompt = self.max_experiences_in_prompt.clamp(1, 5);
        self.max_attempts = self.max_attempts.clamp(1, 10);
        self.max_execution_secs = self.max_execution_secs.clamp(10, 3600);
        self.token_budget = self.token_budget.clamp(1_000, 500_000);
        self.cost_budget_usd = unit(self.cost_budget_usd, 0.0, 1000.0);
        self.max_tool_calls = self.max_tool_calls.clamp(1, 200);
        self.max_background_jobs = self.max_background_jobs.clamp(1, 16);
        self.stale_after_days = self.stale_after_days.clamp(1, 3650);
        self.practice_max_per_day = self.practice_max_per_day.clamp(1, 24);
        self.practice_min_interval_mins = self.practice_min_interval_mins.clamp(5, 1440);
        self.practice_idle_mins = self.practice_idle_mins.clamp(1, 240);
        self.practice_max_steps = self.practice_max_steps.clamp(2, 10);
        self
    }

    /// New experiences may be created only when the master switch and automatic creation are on.
    pub fn may_learn(&self) -> bool {
        self.learning_enabled && self.auto_memory_creation
    }

    /// Experiences may be shown to the agent only when retrieval is on. The master switch turns
    /// off *learning*; what is already known can still be used unless retrieval is switched off.
    pub fn may_retrieve(&self) -> bool {
        self.retrieval_enabled
    }

    /// Practising on its own needs the master switch and its own switch (off by default).
    pub fn may_practice(&self) -> bool {
        self.learning_enabled && self.auto_practice_enabled
    }

    /// Bounded experimentation needs both the master switch and its own switch.
    pub fn may_experiment(&self) -> bool {
        self.learning_enabled && self.autonomous_experimentation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_learn_and_retrieve_but_do_not_experiment() {
        let s = LearningSettings::default();
        assert!(s.may_learn() && s.may_retrieve() && !s.may_experiment());
        assert_eq!(s.clone().clamped(), s);
    }

    #[test]
    fn the_switches_are_independent() {
        let mut s = LearningSettings::default();
        s.learning_enabled = false;
        assert!(!s.may_learn() && !s.may_experiment(), "master off stops learning and experiments");
        assert!(s.may_retrieve(), "what is already known can still be used");
        s.learning_enabled = true;
        s.retrieval_enabled = false;
        assert!(s.may_learn() && !s.may_retrieve());
        s.autonomous_experimentation = true;
        assert!(s.may_experiment(), "experimenting is its own switch");
        s.auto_memory_creation = false;
        assert!(!s.may_learn() && s.may_experiment());
    }

    #[test]
    fn limits_cannot_be_removed() {
        let mut s = LearningSettings::default();
        s.max_attempts = 0;
        s.max_execution_secs = 10_000_000;
        s.token_budget = u32::MAX;
        s.similarity_threshold = f32::NAN;
        s.cost_budget_usd = -5.0;
        s.max_tool_calls = 0;
        let c = s.clamped();
        assert_eq!((c.max_attempts, c.max_execution_secs, c.token_budget, c.max_tool_calls), (1, 3600, 500_000, 1));
        assert!(c.similarity_threshold >= 0.05 && c.cost_budget_usd == 0.0);
    }
}
