//! Feature Modules Subsystem

#[cfg(feature = "unwired")]
pub mod backup_manager;
pub mod agent_experience;
pub mod browser_agent;
pub mod code_verification;
pub mod coding_session;
pub mod coding_runner;
pub mod coding_driver;
pub mod coding_session_record;
pub mod run_profile;
pub mod project_brief;
pub mod context;
pub mod decision;
#[cfg(test)]
mod code_verification_tests;
pub mod practice_runner;
#[cfg(feature = "unwired")]
pub mod code_generator;
#[cfg(feature = "unwired")]
pub mod dependency_integrator;
#[cfg(feature = "unwired")]
pub mod document_reader;
#[cfg(feature = "unwired")]
pub mod file_organizer;
#[cfg(feature = "unwired")]
pub mod image_generator;
pub mod llm_router;
pub mod planner;
pub mod reader;
pub mod stuck;
#[cfg(feature = "unwired")]
pub mod scheduler;
#[cfg(feature = "unwired")]
pub mod updater;
