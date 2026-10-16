//! Vision, Document OCR, Face Biometrics & Object Detection Adapter
//!
//! `screen` is the part that does something: it reads the text on the screen with Windows' own text reader (card R2-07). `vision_engine` is a simulator that returns
//! fixed text (see the register, "Code that pretends").

pub mod screen;
#[cfg(feature = "unwired")]
pub mod vision_engine;

#[cfg(feature = "unwired")]
pub use vision_engine::{DetectedObject, OcrResult, VisionEngine};
