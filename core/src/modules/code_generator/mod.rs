//! Code Generator Module

pub mod tdd_engine;

pub use tdd_engine::{CanaryProbe, TddEngine, TddReport, TddStage};

pub fn status() -> &'static str {
    "Code Generator Ready"
}

