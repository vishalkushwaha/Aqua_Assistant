//! Agent Experience system: verified, reusable ways of solving tasks that the agent remembers and
//! reuses when a similar task comes up.
//!
//! This sits beside the recipe store (`solutions.db`), which replays exact tool sequences. An
//! experience is broader: the problem, the approach, the important steps, how it was verified and
//! how it has performed when reused. Nothing here is on the agent's critical path: if the store, the
//! index or the worker fails, the agent carries on without memory.

pub mod budget;
pub mod learner;
#[cfg(test)]
mod learner_tests;
pub mod model;
pub mod quality;
pub mod sanitize;
pub mod service;
#[cfg(test)]
mod service_tests;
pub mod session_summary;
pub mod settings;
pub mod practice;
#[cfg(test)]
mod practice_tests;
pub mod retrieval;
#[cfg(test)]
mod retrieval_tests;
pub mod store;
#[cfg(test)]
mod store_tests;
pub mod vector;

pub use model::{Experience, ExperienceScope, ExperienceStatus, ExperienceType, Verification, VerificationMethod};
pub use settings::LearningSettings;
pub use store::{ExperienceStore, ListFilter, Viewer};
