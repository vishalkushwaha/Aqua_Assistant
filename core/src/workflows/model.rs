//! What a workflow is: a trigger and a list of steps, plus the record of each run.
//!
//! Everything a person can write is validated here before it is stored, so the engine can trust it.

use super::schedule::CronExpr;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Aqua has one local user today; every stored row still carries its owner and every query filters
/// on it, so a real user identity only has to change this one function.
pub fn current_owner() -> &'static str {
    "local"
}

pub const MAX_STEPS: usize = 20;
pub const MAX_RETRIES: u32 = 5;
pub const MIN_INTERVAL_SECS: u64 = 60;
/// A step's default time limit (an agent step is bounded by its own step limit instead).
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Only when a person presses Run.
    Manual,
    /// A 5-field cron expression in the computer's local time, e.g. `0 9 * * *` (every day at 9 AM).
    Schedule { cron: String },
    /// Every N seconds (at least 60).
    Interval { every_secs: u64 },
    /// Once each time Aqua starts.
    Startup,
    /// When `POST /api/v1/workflows/:id/trigger` is called with the API token; the body is
    /// available to the steps as `trigger.payload`.
    Webhook,
}

impl Trigger {
    pub fn kind(&self) -> &'static str {
        match self {
            Trigger::Manual => "manual",
            Trigger::Schedule { .. } => "schedule",
            Trigger::Interval { .. } => "interval",
            Trigger::Startup => "startup",
            Trigger::Webhook => "webhook",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepKind {
    /// One call to a tool of the guarded tool host (`fs.read`, `web.get`, `git.status`, ...).
    Tool {
        tool: String,
        #[serde(default)]
        arguments: Value,
    },
    /// A goal for the Aqua agent (browser and tools), for work that needs judgement.
    Agent {
        goal: String,
        #[serde(default = "default_agent_steps")]
        max_steps: u32,
    },
    /// Compose text from earlier results. The rendered text is this step's output.
    Set { value: String },
}

fn default_agent_steps() -> u32 {
    8
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    /// The run stops and the remaining steps are skipped.
    #[default]
    Stop,
    /// The failure is recorded and the run goes on with the next step.
    Continue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// Short name used to refer to this step's result: `{{steps.<id>.output}}`.
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub kind: StepKind,
    /// The step runs only if this renders to something other than empty, `false`, `0`, `no`.
    #[serde(default)]
    pub only_if: Option<String>,
    #[serde(default)]
    pub retries: u32,
    #[serde(default = "default_retry_delay")]
    pub retry_delay_secs: u64,
    #[serde(default)]
    pub on_failure: OnFailure,
    /// 0 means the default.
    #[serde(default)]
    pub timeout_secs: u64,
}

fn default_retry_delay() -> u64 {
    5
}

/// What a person sends to create or replace a workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub enabled: bool,
    pub trigger: Trigger,
    pub steps: Vec<Step>,
    /// If Aqua was not running when a scheduled run was due: run it once when it starts again
    /// (`true`), or skip it (default).
    #[serde(default)]
    pub catch_up: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub id: i64,
    pub owner_id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub trigger: Trigger,
    pub steps: Vec<Step>,
    pub catch_up: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// Unix seconds of the next scheduled run (`None` for triggers that are not timed, or disabled).
    pub next_run_at: Option<i64>,
    pub last_run_at: i64,
    pub last_status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    /// Finished, but a step that was set to continue on failure failed.
    Partial,
    Failed,
    Cancelled,
    /// Aqua stopped while it was running.
    Interrupted,
    /// Not run: the previous run was still going, or the run was missed while Aqua was closed.
    Skipped,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Partial => "partial",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Skipped => "skipped",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => RunStatus::Running,
            "succeeded" => RunStatus::Succeeded,
            "partial" => RunStatus::Partial,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            "skipped" => RunStatus::Skipped,
            _ => RunStatus::Interrupted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRun {
    pub idx: u32,
    pub step_id: String,
    pub name: String,
    /// pending | running | succeeded | failed | skipped
    pub status: String,
    pub attempts: u32,
    pub started_at: i64,
    pub finished_at: i64,
    /// What the step produced, redacted and capped.
    pub output: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: i64,
    pub workflow_id: i64,
    pub owner_id: String,
    pub trigger_kind: String,
    pub status: RunStatus,
    pub started_at: i64,
    pub finished_at: i64,
    pub error: String,
    pub steps: Vec<StepRun>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A value that must never be stored in a workflow: it would sit in the database in plain text and be
/// shown in the UI. Secrets belong in the OS credential store; a step can use a tool that reads them.
/// Returns the path of the first one found.
pub(crate) fn secret_in(v: &Value, path: &str) -> Option<String> {
    match v {
        Value::Object(m) => m.iter().find_map(|(k, x)| {
            let here = format!("{path}.{k}");
            if aqua_tool_host::output::is_secret_key(k) && x.as_str().is_some_and(|s| !s.trim().is_empty() && !s.contains("{{")) {
                Some(here)
            } else {
                secret_in(x, &here)
            }
        }),
        Value::Array(a) => a.iter().enumerate().find_map(|(i, x)| secret_in(x, &format!("{path}[{i}]"))),
        // A placeholder such as `?token={{trigger.payload.t}}` is a reference, not a stored secret.
        Value::String(s) if s.contains("{{") => None,
        Value::String(s) => {
            // An email address is personal, not a credential (a form-filling step legitimately types one).
            let without_emails = s.split_whitespace().filter(|w| !w.find('@').is_some_and(|i| i > 0 && w[i..].contains('.') && !w.contains("://"))).collect::<Vec<_>>().join(" ");
            let (clean, n) = crate::memory::experience::sanitize::redact(&without_emails);
            (n > 0 && clean != without_emails).then(|| path.to_string())
        }
        _ => None,
    }
}

impl WorkflowInput {
    /// Check everything a person wrote. The message names the problem in plain words.
    pub fn validate(&self) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err("the name must be 1 to 80 characters".into());
        }
        if self.description.chars().count() > 500 {
            return Err("the description must be at most 500 characters".into());
        }
        match &self.trigger {
            Trigger::Schedule { cron } => {
                CronExpr::parse(cron).map_err(|e| format!("the schedule is not valid: {e}"))?;
            }
            Trigger::Interval { every_secs } if *every_secs < MIN_INTERVAL_SECS => {
                return Err(format!("an interval must be at least {MIN_INTERVAL_SECS} seconds"));
            }
            _ => {}
        }
        if self.steps.is_empty() {
            return Err("a workflow needs at least one step".into());
        }
        if self.steps.len() > MAX_STEPS {
            return Err(format!("a workflow can have at most {MAX_STEPS} steps"));
        }
        let mut seen: Vec<&str> = Vec::new();
        for (i, s) in self.steps.iter().enumerate() {
            let at = format!("step {} ({})", i + 1, s.id);
            if !valid_id(&s.id) {
                return Err(format!("step {}: the id must be 1 to 32 characters of a-z, 0-9, _ or -", i + 1));
            }
            if seen.contains(&s.id.as_str()) {
                return Err(format!("{at}: another step already uses this id"));
            }
            seen.push(&s.id);
            if s.retries > MAX_RETRIES {
                return Err(format!("{at}: at most {MAX_RETRIES} retries"));
            }
            if s.retry_delay_secs > 3600 || s.timeout_secs > 86_400 {
                return Err(format!("{at}: the retry delay is at most an hour and the time limit at most a day"));
            }
            if let Some(cond) = s.only_if.as_deref().filter(|c| !c.trim().is_empty()) {
                crate::workflows::template::check_condition(cond).map_err(|e| format!("{at}: the condition {e}"))?;
            }
            match &s.kind {
                StepKind::Tool { tool, arguments } => {
                    if tool.trim().is_empty() || tool.len() > 64 || !tool.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
                        return Err(format!("{at}: the tool name is not valid"));
                    }
                    if !(arguments.is_null() || arguments.is_object()) {
                        return Err(format!("{at}: the arguments must be an object"));
                    }
                    if let Some(p) = secret_in(arguments, "arguments") {
                        return Err(format!("{at}: {p} looks like a password or token. Do not store secrets in a workflow: it is saved in plain text. Use a tool that reads it from the credential store."));
                    }
                }
                StepKind::Agent { goal, max_steps } => {
                    if goal.trim().is_empty() || goal.chars().count() > 2000 {
                        return Err(format!("{at}: the goal must be 1 to 2000 characters"));
                    }
                    let most = crate::modules::browser_agent::max_rounds_for_goal(goal);
                    if !(1..=most).contains(max_steps) {
                        return Err(format!("{at}: the agent step limit is 1 to {most}"));
                    }
                    if secret_in(&Value::String(goal.clone()), "goal").is_some() {
                        return Err(format!("{at}: the goal looks like it contains a password or token; do not store secrets in a workflow"));
                    }
                }
                StepKind::Set { value } => {
                    if value.chars().count() > 5000 {
                        return Err(format!("{at}: the text is at most 5000 characters"));
                    }
                }
            }
        }
        Ok(())
    }
}
