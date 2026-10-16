//! Learning & Reinforcement Learning Directory Engine

pub mod rl_engine;

pub use rl_engine::{PolicyWeight, RlEngine, TrajectoryStep};

pub fn status() -> &'static str {
    "RL Learning Engine Ready"
}
