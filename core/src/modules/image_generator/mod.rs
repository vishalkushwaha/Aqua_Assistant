//! Image Generator Module

pub mod image_engine;

pub use image_engine::{GeneratedImage, ImageEngine, ImageGenerationRequest};

pub fn status() -> &'static str {
    "Image Generator Ready"
}

