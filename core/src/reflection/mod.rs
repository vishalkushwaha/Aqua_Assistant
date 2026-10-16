//! Self-Correction Reflection Subsystem

pub mod reflection_engine;

pub use reflection_engine::{CrashReport, ReflectionEngine};

pub fn status() -> &'static str {
    "Reflection Engine Ready"
}
