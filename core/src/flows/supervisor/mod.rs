//! The supervisor: what a finished or failing run *means*, decided from evidence and never from hope.
//!
//! - `classify`: what kind of failure this is (authentication, network, a page that changed, ...).
//! - `ladder`: what to do about it, in order: retry, another way, a diagnosis, ask the person, stop safely.
//! - `verdict`: how a run really ended: success, recovered, failed, unknown or waiting, with the evidence.
//!
//! Nothing here changes a flow. Recovery that edits anything is a later layer and always goes through the
//! same commands and approvals a person's edit does.

pub mod assistant;
pub mod classify;
pub mod health;
pub mod ladder;
pub mod recovery;
pub mod verdict;

pub use classify::{classify, ErrorClass};
pub use ladder::{decide, Action, Situation};
pub use verdict::{judge, Verdict, VerdictKind};
