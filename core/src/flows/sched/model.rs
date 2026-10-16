//! A schedule: when a flow runs by itself, in which time zone, with which inputs, what to do if a time
//! was missed, and who to tell about what.
//!
//! A schedule is data in the database, not a timer in memory, so it survives the app closing, a restart
//! and a crash. `next_run_at` is a unix time (zone-free); the zone is only used to work out the next one.

use crate::flows::zone::Zone;
use crate::workflows::schedule::CronExpr;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A scheduled time this far past counts as missed (a scheduler that is a little slow is not a miss).
pub const GRACE_SECS: i64 = 300;

pub fn yes() -> bool {
    true
}

/// What to do when a scheduled time passed while Aqua was not able to run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Misfire {
    /// Run it now, as soon as Aqua is back.
    RunNow,
    /// Do not run it; the schedule carries on at its next time.
    Skip,
    /// Do not run it, but ask. The choice waits in the inbox.
    #[default]
    Ask,
    /// Same as skip: wait for the next scheduled time (named separately because it reads differently).
    NextRun,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum When {
    /// A cron expression on the zone's wall clock: `0 10 * * *` is every day at 10:00.
    Cron { cron: String },
    /// One time only: `YYYY-MM-DD HH:MM` on the zone's wall clock.
    Once { when: String },
}

/// Which events get a notification, and where it goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyPrefs {
    #[serde(default = "yes")]
    pub on_start: bool,
    #[serde(default = "yes")]
    pub on_success: bool,
    #[serde(default = "yes")]
    pub on_failure: bool,
    /// A step failed and Aqua fixed it by itself.
    #[serde(default = "yes")]
    pub on_recovered: bool,
    /// The run is waiting for the person (an approval, a question, a decision).
    #[serde(default = "yes")]
    pub on_needs_you: bool,
    #[serde(default)]
    pub on_each_step: bool,
    /// Where: `in_app` always; `email`, `slack`, `push` once those accounts are connected.
    #[serde(default = "default_channels")]
    pub channels: Vec<String>,
}

fn default_channels() -> Vec<String> {
    vec!["in_app".into()]
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        Self { on_start: true, on_success: true, on_failure: true, on_recovered: true, on_needs_you: true, on_each_step: false, channels: default_channels() }
    }
}

/// What makes a schedule a campaign: it stops by itself, and may take different inputs each time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Campaign {
    /// The last day it may run, `YYYY-MM-DD`, in the schedule's zone (the whole day counts).
    #[serde(default)]
    pub end_on: Option<String>,
    /// Stop after this many runs have been started.
    #[serde(default)]
    pub max_runs: Option<u32>,
    /// Inputs that take turns: the first run uses the first entry, the second the second, and so on, round again
    /// from the start. Each entry is laid over the schedule's `inputs`.
    #[serde(default)]
    pub series: Vec<Map<String, Value>>,
}

pub const MAX_CAMPAIGN_RUNS: u32 = 1000;
pub const MAX_SERIES: usize = 200;

impl Campaign {
    pub fn is_set(&self) -> bool {
        self.end_on.is_some() || self.max_runs.is_some()
    }
}

/// What a person sends to create or replace a schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleInput {
    pub name: String,
    pub when: When,
    /// An IANA name (`Asia/Kolkata`), an offset (`+05:30`), `UTC`, or empty for this computer's zone.
    #[serde(default)]
    pub timezone: String,
    /// The flow's inputs for every run (`{"topic": "AI agents"}`).
    #[serde(default)]
    pub inputs: Map<String, Value>,
    #[serde(default)]
    pub misfire: Misfire,
    #[serde(default)]
    pub notify: NotifyPrefs,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub campaign: Campaign,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: i64,
    pub owner_id: String,
    pub flow_id: i64,
    pub name: String,
    pub when: When,
    pub timezone: String,
    pub inputs: Map<String, Value>,
    pub misfire: Misfire,
    pub notify: NotifyPrefs,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// The next moment it is due (`None`: nothing more to run, or switched off).
    pub next_run_at: Option<i64>,
    pub last_run_at: i64,
    pub last_run_id: Option<i64>,
    /// How the last run ended, or why it did not happen.
    pub last_status: String,
    pub campaign: Campaign,
    /// The end of the last day it may run, as a unix time.
    pub ends_at: Option<i64>,
    /// How many runs it has started (a run you start by hand does not count).
    pub runs_started: u32,
}

/// One time a schedule came due, and what was done about it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Occurrence {
    pub id: i64,
    pub schedule_id: i64,
    pub due_at: i64,
    pub decided_at: i64,
    /// started | started_late | skipped | missed_pending | failed_to_start | resolved_run | resolved_skip
    pub outcome: String,
    pub run_id: Option<i64>,
    /// In words: why it was skipped, or why it could not start.
    pub reason: String,
}

/// Where a schedule is in its life, for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Switched off.
    Paused,
    /// Waiting for its next time.
    Scheduled,
    /// A run it started is going (or waiting for you).
    Running,
    /// A missed time is waiting for your decision, or the last run failed.
    NeedsAttention,
    /// A one-time schedule that has run.
    Completed,
    /// The flow has problems, so it cannot run.
    Invalid,
}

impl ScheduleInput {
    /// Check what a person wrote. Returns the zone and the first time it is due after `now`.
    pub fn validate(&self, now: i64) -> Result<(Zone, Option<i64>), String> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err("the schedule needs a name of 1 to 80 characters".into());
        }
        let zone = Zone::parse(&self.timezone)?;
        let next = match &self.when {
            When::Cron { cron } => {
                let c = CronExpr::parse(cron).map_err(|e| format!("the schedule is not valid: {e}"))?;
                Some(zone.next_cron(&c, now).ok_or("that schedule never comes")?)
            }
            When::Once { when } => {
                let naive = NaiveDateTime::parse_from_str(when.trim(), "%Y-%m-%d %H:%M").map_err(|_| format!("\"{when}\" is not a date and time; write it like 2026-10-01 10:00"))?;
                let ts = zone.to_timestamp(naive).ok_or_else(|| format!("\"{when}\" does not exist in {} (the clocks change then)", zone.label()))?;
                if ts <= now {
                    return Err(format!("{when} in {} has already passed", zone.label()));
                }
                Some(ts)
            }
        };
        self.validate_campaign(&zone, now, next)?;
        for c in &self.notify.channels {
            if !matches!(c.as_str(), "in_app" | "email" | "slack" | "push" | "voice") {
                return Err(format!("\"{c}\" is not a notification channel (in_app, email, slack, push, voice)"));
            }
        }
        if serde_json::to_string(&self.inputs).map(|s| s.len()).unwrap_or(usize::MAX) > 16 * 1024 {
            return Err("the inputs are too large".into());
        }
        Ok((zone, next))
    }
}

impl ScheduleInput {
    /// The end of the last day of a campaign as a unix time, in `zone`.
    pub fn ends_at(&self, zone: &Zone) -> Result<Option<i64>, String> {
        let Some(day) = self.campaign.end_on.as_deref().map(str::trim).filter(|d| !d.is_empty()) else { return Ok(None) };
        let date = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| format!("\"{day}\" is not a date; write the last day like 2026-12-31"))?;
        let end = date.and_hms_opt(23, 59, 59).ok_or("that date is not valid")?;
        zone.to_timestamp(end).or_else(|| zone.to_timestamp(end - chrono::Duration::hours(1))).map(Some).ok_or_else(|| format!("{day} cannot be worked out in {}", zone.label()))
    }

    fn validate_campaign(&self, zone: &Zone, now: i64, first: Option<i64>) -> Result<(), String> {
        let c = &self.campaign;
        if let Some(m) = c.max_runs {
            if !(1..=MAX_CAMPAIGN_RUNS).contains(&m) {
                return Err(format!("a campaign runs from 1 to {MAX_CAMPAIGN_RUNS} times, not {m}"));
            }
        }
        if let Some(end) = self.ends_at(zone)? {
            if end <= now {
                return Err(format!("the last day ({}) has already passed", c.end_on.clone().unwrap_or_default()));
            }
            if first.is_some_and(|f| f > end) {
                return Err("the first run would come after the last day, so the campaign would never run".into());
            }
        }
        if matches!(self.when, When::Once { .. }) && c.is_set() {
            return Err("a one-time schedule has nothing to repeat: leave the last day and the number of runs empty".into());
        }
        if c.series.len() > MAX_SERIES {
            return Err(format!("at most {MAX_SERIES} different inputs can take turns"));
        }
        if c.series.iter().any(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(usize::MAX) > 16 * 1024) || serde_json::to_string(&c.series).map(|s| s.len()).unwrap_or(usize::MAX) > 256 * 1024 {
            return Err("the inputs that take turns are too large".into());
        }
        Ok(())
    }
}

impl Schedule {
    /// The inputs for the next run: the schedule's own, with the next entry of the series laid over them.
    pub fn inputs_for_next_run(&self) -> Map<String, Value> {
        let mut m = self.inputs.clone();
        if !self.campaign.series.is_empty() {
            for (k, v) in &self.campaign.series[self.runs_started as usize % self.campaign.series.len()] {
                m.insert(k.clone(), v.clone());
            }
        }
        m
    }

    /// Has a campaign made every run it was to make, or has its last day gone?
    pub fn campaign_over(&self, at: i64) -> bool {
        self.campaign.max_runs.is_some_and(|m| self.runs_started >= m) || self.ends_at.is_some_and(|e| at > e)
    }

    /// The next time after `after`, stopping at a campaign's last day or its number of runs. `starting` says a run is
    /// being started right now, which counts towards the number.
    pub fn next_within_campaign(&self, after: i64, starting: bool) -> Option<i64> {
        let next = self.next_after(after)?;
        if self.ends_at.is_some_and(|e| next > e) {
            return None;
        }
        if self.campaign.max_runs.is_some_and(|m| self.runs_started + starting as u32 >= m) {
            return None;
        }
        Some(next)
    }

    pub fn zone(&self) -> Result<Zone, String> {
        Zone::parse(&self.timezone)
    }

    /// The next time after `after`, or `None` for a one-time schedule (which has just had its turn).
    pub fn next_after(&self, after: i64) -> Option<i64> {
        match &self.when {
            When::Cron { cron } => self.zone().ok()?.next_cron(&CronExpr::parse(cron).ok()?, after),
            When::Once { .. } => None,
        }
    }

    /// "every day at 10:00" is not derivable from cron in general, so this says what is stored.
    pub fn describe(&self) -> String {
        match &self.when {
            When::Cron { cron } => format!("cron {cron} ({})", self.zone().map(|z| z.label()).unwrap_or_else(|e| e)),
            When::Once { when } => format!("once at {when} ({})", self.zone().map(|z| z.label()).unwrap_or_else(|e| e)),
        }
    }
}
