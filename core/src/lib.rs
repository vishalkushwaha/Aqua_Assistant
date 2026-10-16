//! Aqua Core Engine Library

pub mod api_gateway;
#[cfg(feature = "unwired")]
pub mod career;
pub mod crypto;
#[cfg(feature = "unwired")]
pub mod hal;
pub mod laya_validator;
#[path = "../../learning/mod.rs"]
pub mod learning;
pub mod memory;
pub mod model;
pub mod model_providers;
pub mod modules;
pub mod pipeline;
pub mod reflection;
pub mod runtime;
pub mod startup_health;
pub mod storage;
pub mod agent_runs;
pub mod agent_tools;
pub mod agents;
pub mod autonomy;
pub mod door_shadow;
pub mod bm25;
pub mod conversation;
pub mod audit_key;
pub mod dictation;
pub mod hooks;
pub mod config_files;
pub mod flags;
pub mod skills_inbox;
pub mod supervise;
pub mod jobs;
pub mod logging;
pub mod steer;
pub mod vault;
pub mod promoted;
pub mod reminders;
pub mod queue;
pub mod skills;
pub mod tool_search;
pub mod metrics;
pub mod project_instructions;
pub mod project_pack;
pub mod project_context;
pub mod front_matter;
pub mod commands;
pub mod command_tools;
pub mod company_brain;
pub mod company_ingest;
pub mod company_memory;
pub mod company_retrieval;
pub mod headless;
pub mod cli_tools;
pub mod prompt_text;
pub mod sources;
pub mod research;
pub mod doctor;
#[cfg(test)]
mod docs_gen;
/// Shared test scaffolding: the scripted planner, completer and approver, the environment guard, the throw-away world and `wait_until`.
/// Compiled for this crate's own tests; the `test-support` feature makes it available to integration tests. Never in a release build.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod redteam;
#[cfg(test)]
mod redteam_door;
#[cfg(test)]
mod one_door_table;
#[cfg(test)]
mod one_door_enforce_tests;
#[cfg(test)]
mod redteam_files;
pub mod protected_file;
pub mod mcp_sandbox;
pub mod mcp_exposure;
pub mod mcp_remote;
pub mod config_reload;
pub mod tool_host;
pub mod tool_providers;
pub mod full_access;
pub mod voice_commands;
pub mod follow_up;
pub mod local_facts;
pub mod live_weather;
pub mod speech_stream;
pub mod temp_sweep;
pub mod spoken_request;
pub mod spoken_time;
pub mod turn_timing;
pub mod flows;
pub mod workflows;
pub mod self_repair;
pub mod triage;
pub mod readback;
pub mod project_terms;
pub mod coding_voice;

pub use pipeline::{
    collect_stage_health, run_text_to_speech, run_wav_file_to_speech,
    run_text_to_speech_streaming, run_text_to_speech_with_mcp, run_wav_file_to_speech_streaming,
    run_wav_file_to_speech_with_mcp, AquaEndToEndPipeline, Listener, PipelineStageHealth,
    PipelineTextResult, PipelineWavResult, TurnEvent,
};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
