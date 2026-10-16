//! Scheduler & Morning Briefing Module

pub mod scheduler_engine;

pub use scheduler_engine::{ScheduleType, ScheduledTask, SchedulerEngine};

pub fn status() -> &'static str {
    "Scheduler Ready"
}

