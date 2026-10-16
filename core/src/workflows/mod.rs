//! Workflow automation: a person describes something to do (a trigger and steps), and Aqua does it
//! by itself, on a schedule or on demand, and keeps a log of every run.
//!
//! Everything is local. The pieces:
//! - `model`: what a workflow is, and the checks on what a person writes.
//! - `schedule`: cron expressions ("every day at 9 AM") in local time.
//! - `template`: `{{steps.fetch.output}}`, how a step uses earlier results.
//! - `store`: SQLite (`data/workflows.db`): workflows, runs, per-step logs. Scoped by owner.
//! - `engine`: runs the steps in order, with retries and a failure policy.
//! - `runner`: the scheduler thread, single-flight per workflow, missed-run policy, crash recovery.
//! - `live`: the real executor (guarded tool host and the Aqua agent).
//!
//! See `documentation/WORKFLOWS.md`.

pub mod engine;
pub mod live;
pub mod model;
pub mod runner;
pub mod schedule;
pub mod store;
pub mod template;

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod live_taint_tests;
#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod runner_tests;
#[cfg(test)]
mod schedule_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod template_tests;
