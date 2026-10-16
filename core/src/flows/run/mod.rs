//! Phase 3: running flows.
//!
//! - `model`: a run, its steps, waits, events.
//! - `store`: all of it in SQLite, written as it happens, plus the ledger that stops an outside action
//!   being done twice.
//! - `exec`: what does the work of a step, by kind; the logic steps are the engine's own.
//! - `engine`: the driver: which steps are ready, run independent ones together, wait for people
//!   without holding a thread, skip what a decision made unreachable.
//! - `manager`: start, answer, pause, resume, stop, retry, skip, run from a step, wake on time.

pub mod engine;
pub mod exec;
pub mod manager;
pub mod model;
pub mod store;
pub mod recovery_store;
