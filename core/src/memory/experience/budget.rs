//! Hard limits for autonomous experimentation.
//!
//! Every attempt, tool call and token is counted, and the loop stops the moment any limit is
//! reached. There is no "unlimited": `LearningSettings::clamped` never lets a limit be removed.

use super::settings::LearningSettings;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    MaxAttempts,
    MaxTime,
    MaxTokens,
    MaxCost,
    MaxToolCalls,
}

impl StopReason {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::MaxAttempts => "reached the maximum number of attempts",
            Self::MaxTime => "reached the maximum execution time",
            Self::MaxTokens => "reached the token budget",
            Self::MaxCost => "reached the cost budget",
            Self::MaxToolCalls => "reached the maximum number of tool calls",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    pub max_attempts: u32,
    pub max_time: Duration,
    pub max_tokens: u32,
    pub max_cost_usd: f32,
    pub max_tool_calls: u32,
}

impl Budget {
    pub fn from_settings(s: &LearningSettings) -> Self {
        let s = s.clone().clamped();
        Self {
            max_attempts: s.max_attempts,
            max_time: Duration::from_secs(s.max_execution_secs as u64),
            max_tokens: s.token_budget,
            max_cost_usd: s.cost_budget_usd,
            max_tool_calls: s.max_tool_calls,
        }
    }
}

/// Usage so far. Call `check` before each attempt and after each step.
#[derive(Debug, Clone)]
pub struct BudgetTracker {
    pub budget: Budget,
    started: Instant,
    pub attempts: u32,
    pub tokens: u32,
    pub tool_calls: u32,
    pub llm_calls: u32,
    pub cost_usd: f32,
}

/// What an experimentation run used, for the dashboard and the evaluation comparison.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub attempts: u32,
    pub tokens_used: u32,
    pub llm_calls: u32,
    pub tool_calls: u32,
    pub execution_ms: u64,
    pub estimated_cost_usd: f32,
    pub stopped_by: Option<StopReason>,
}

impl BudgetTracker {
    pub fn new(budget: Budget) -> Self {
        Self { budget, started: Instant::now(), attempts: 0, tokens: 0, tool_calls: 0, llm_calls: 0, cost_usd: 0.0 }
    }

    /// Whether another attempt may begin. Counts it when it may.
    pub fn start_attempt(&mut self) -> Result<(), StopReason> {
        self.check()?;
        if self.attempts >= self.budget.max_attempts {
            return Err(StopReason::MaxAttempts);
        }
        self.attempts += 1;
        Ok(())
    }

    pub fn add_tokens(&mut self, n: u32) {
        self.tokens = self.tokens.saturating_add(n);
        self.llm_calls += 1;
    }

    pub fn add_tool_calls(&mut self, n: u32) {
        self.tool_calls = self.tool_calls.saturating_add(n);
    }

    pub fn add_cost(&mut self, usd: f32) {
        if usd.is_finite() && usd > 0.0 {
            self.cost_usd += usd;
        }
    }

    /// The first limit that has been reached, if any. A cost budget of zero means "no paid calls
    /// allowed": local models cost nothing, so it only trips if something reports a cost.
    pub fn check(&self) -> Result<(), StopReason> {
        if self.started.elapsed() >= self.budget.max_time {
            return Err(StopReason::MaxTime);
        }
        if self.tokens >= self.budget.max_tokens {
            return Err(StopReason::MaxTokens);
        }
        if self.tool_calls >= self.budget.max_tool_calls {
            return Err(StopReason::MaxToolCalls);
        }
        if self.cost_usd > self.budget.max_cost_usd {
            return Err(StopReason::MaxCost);
        }
        Ok(())
    }

    pub fn remaining_time(&self) -> Duration {
        self.budget.max_time.saturating_sub(self.started.elapsed())
    }

    pub fn usage(&self, stopped_by: Option<StopReason>) -> Usage {
        Usage {
            attempts: self.attempts,
            tokens_used: self.tokens,
            llm_calls: self.llm_calls,
            tool_calls: self.tool_calls,
            execution_ms: self.started.elapsed().as_millis() as u64,
            estimated_cost_usd: self.cost_usd,
            stopped_by,
        }
    }
}

/// A rough token count for budgeting (about four characters per token).
pub fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() as u32).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(f: impl FnOnce(&mut LearningSettings)) -> BudgetTracker {
        let mut s = LearningSettings::default();
        f(&mut s);
        BudgetTracker::new(Budget::from_settings(&s))
    }

    #[test]
    fn attempts_are_capped() {
        let mut t = tracker(|s| s.max_attempts = 2);
        assert!(t.start_attempt().is_ok() && t.start_attempt().is_ok());
        assert_eq!(t.start_attempt(), Err(StopReason::MaxAttempts));
        assert_eq!(t.attempts, 2);
    }

    #[test]
    fn tokens_tool_calls_and_cost_each_stop_the_run() {
        let mut t = tracker(|s| s.token_budget = 1_000);
        t.add_tokens(999);
        assert!(t.check().is_ok());
        t.add_tokens(2);
        assert_eq!(t.check(), Err(StopReason::MaxTokens));
        assert_eq!(t.start_attempt(), Err(StopReason::MaxTokens), "no new attempt once a limit is hit");

        let mut t = tracker(|s| s.max_tool_calls = 3);
        t.add_tool_calls(3);
        assert_eq!(t.check(), Err(StopReason::MaxToolCalls));

        let mut t = tracker(|s| s.cost_budget_usd = 0.10);
        t.add_cost(0.05);
        assert!(t.check().is_ok());
        t.add_cost(0.06);
        assert_eq!(t.check(), Err(StopReason::MaxCost));
        // Free local inference never trips a zero budget; a reported cost does.
        let mut free = tracker(|s| s.cost_budget_usd = 0.0);
        free.add_tokens(10);
        assert!(free.check().is_ok());
        free.add_cost(0.01);
        assert_eq!(free.check(), Err(StopReason::MaxCost));
    }

    #[test]
    fn time_is_capped_and_a_limit_cannot_be_configured_away() {
        let mut t = BudgetTracker::new(Budget { max_attempts: 5, max_time: Duration::from_millis(30), max_tokens: 1_000, max_cost_usd: 0.0, max_tool_calls: 10 });
        assert!(t.check().is_ok());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(t.check(), Err(StopReason::MaxTime));
        assert_eq!(t.remaining_time(), Duration::ZERO);
        assert_eq!(t.start_attempt(), Err(StopReason::MaxTime));

        let mut s = LearningSettings::default();
        s.max_attempts = 0;
        s.max_execution_secs = u32::MAX;
        let b = Budget::from_settings(&s);
        assert!(b.max_attempts >= 1 && b.max_time <= Duration::from_secs(3600));
    }

    #[test]
    fn usage_is_reported_and_tokens_are_estimated() {
        let mut t = tracker(|_| {});
        t.start_attempt().unwrap();
        t.add_tokens(estimate_tokens(&"a".repeat(400)));
        t.add_tool_calls(2);
        let u = t.usage(Some(StopReason::MaxAttempts));
        assert_eq!((u.attempts, u.tokens_used, u.llm_calls, u.tool_calls, u.stopped_by), (1, 100, 1, 2, Some(StopReason::MaxAttempts)));
        assert_eq!(StopReason::MaxTime.describe(), "reached the maximum execution time");
    }
}
