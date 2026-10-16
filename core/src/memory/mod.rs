//! Memory Subsystem
//!
//! Manages vector memory RAG, Solutions DB fast-path macro caching, and long-term experience replay.

pub mod agent_learning;
pub mod embedding;
pub mod experience;
pub mod intent_embed;
pub mod solutions_db;
pub mod static_embed;
pub mod user_facts;

pub use agent_learning::{
    AgentLearning, AgentLearningContext, LearnedRecipe, LearnedToolCall,
};
pub use intent_embed::{intent_embedding, EMBED_DIM};
pub use solutions_db::{PurgeReport, SearchResult, SolutionRecord, SolutionsDb};

pub fn status() -> &'static str {
    "Memory Engine Ready"
}
