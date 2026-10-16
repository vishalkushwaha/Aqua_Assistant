//! AI Flows: a graph of steps that an AI can build from a sentence, a person can edit, and Aqua can run.
//!
//! Phase 1: the model, the node registry, validation, versions and diff, storage, the API and the canvas.
//! Phase 2 (`planner`, `commands`): the AI builder: a sentence becomes a proposal made of validated
//! commands, shown as a diff, applied only when accepted, and undoable.
//! Later phases add the execution engine, browser and platform nodes, spreadsheets, scheduling and n8n.
//! See `documentation/AI_FLOWS.md`.

pub mod browser_exec;
pub mod comment_drafts;
pub mod comments_browser;
pub mod comments_inbox;
pub mod commands;
pub mod db_exec;
pub mod diff;
pub mod image;
pub mod image_exec;
pub mod layout;
pub mod live;
pub mod looping;
pub mod model;
pub mod n8n;
pub mod net;
pub mod notify;
pub mod notify_voice;
pub mod planner;
pub mod platform;
pub mod recipe_builder;
pub mod registry;
pub mod retention;
pub mod run;
pub mod sheet;
pub mod sheet_exec;
pub mod social;
pub mod sched;
pub mod schedule_text;
pub mod services;
pub mod store;
pub mod supervisor;
pub mod tools;
pub mod templates;
pub mod validate;
pub mod zone;

#[cfg(test)]
mod flow_tests;
#[cfg(test)]
mod planner_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod sched_tests;
#[cfg(test)]
mod supervisor_tests;
#[cfg(test)]
mod browser_tests;
#[cfg(test)]
mod image_tests;
#[cfg(test)]
mod social_tests;
#[cfg(test)]
mod facebook_tests;
#[cfg(test)]
mod sheet_tests;
#[cfg(test)]
mod n8n_tests;
#[cfg(test)]
mod comment_tests;
