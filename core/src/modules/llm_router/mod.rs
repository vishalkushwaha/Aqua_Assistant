pub mod draft_model;
pub mod fast_server;
pub mod gpu;
pub mod kv_slots;
pub mod launch_args;
pub mod llama_cli;
pub mod llama_engine;
pub mod llama_server;
pub mod server_options;

pub use llama_cli::GgufModelInfo;
pub use llama_engine::{strip_reasoning_tags, LlamaRouterEngine, StreamedChat};
// What the routes of the gateway say about the models of the roles (card OA9-03). They name them here, as they name the engine, so that the gateway does not need a second module.
pub use crate::model::roles::RoleView;
pub use crate::model::Role;
// What a run's planner replies cost, as the server measured it (card OA9-05): the usage route of the gateway reads it from here, for the same reason.
pub use crate::model::ledger;

pub fn status() -> &'static str {
    "LLM Router Ready"
}
