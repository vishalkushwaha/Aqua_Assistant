//! Phase 5: images for flows.
//!
//! A provider turns a request into a real image file. Nothing here pretends: a provider that is not set up says
//! so, and a step never gets a placeholder that looks like a picture.
//!
//! - `card`: draws a designed text graphic (title, subtitle, colours) as a real PNG. Always available where a
//!   system font exists. For quote cards, announcements and covers; it is not an illustration.
//! - `diffusion`: asks a local image-generation service (Stable Diffusion WebUI / Forge compatible) on this
//!   computer to draw the prompt. Available only when one is set up (`AQUA_IMAGE_ENDPOINT`, loopback only).

pub mod card;
pub mod diffusion;

use std::path::{Path, PathBuf};

/// Smallest and largest side, in pixels, that any provider is asked for.
pub const MIN_SIDE: u32 = 64;
pub const MAX_SIDE: u32 = 4096;

#[derive(Debug, Clone, PartialEq)]
pub struct ImageRequest {
    /// What the picture should show (diffusion).
    pub prompt: String,
    /// The headline on a card.
    pub title: String,
    pub subtitle: String,
    pub width: u32,
    pub height: u32,
    /// A colour scheme name for a card.
    pub style: String,
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageResult {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    /// A short description for people who cannot see the picture (and for platforms that ask for it).
    pub alt_text: String,
    pub provider: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImageError {
    /// The provider needs something that is not there (no service, no font). Says what and how to fix it.
    NotSetUp(String),
    /// The request is wrong (empty prompt, absurd size).
    Invalid(String),
    /// It did not work; `retryable` says whether trying again may.
    Failed { message: String, retryable: bool },
}

impl ImageError {
    pub fn message(&self) -> &str {
        match self {
            ImageError::NotSetUp(m) | ImageError::Invalid(m) => m,
            ImageError::Failed { message, .. } => message,
        }
    }
}

pub trait ImageProvider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Ready to use right now? `Err` says what is missing.
    fn ready(&self) -> Result<(), ImageError>;
    /// Draw the request into `dir/<stem>.png`.
    fn generate(&self, req: &ImageRequest, dir: &Path, stem: &str) -> Result<ImageResult, ImageError>;
}

pub fn check_size(width: u32, height: u32) -> Result<(), ImageError> {
    if !(MIN_SIDE..=MAX_SIDE).contains(&width) || !(MIN_SIDE..=MAX_SIDE).contains(&height) {
        return Err(ImageError::Invalid(format!("invalid setting: the size {width}x{height} is outside {MIN_SIDE} to {MAX_SIDE} pixels")));
    }
    Ok(())
}

/// "1200x627" -> (1200, 627).
pub fn parse_size(text: &str) -> Result<(u32, u32), ImageError> {
    let t: String = text.to_lowercase().replace('×', "x").chars().filter(|c| !c.is_whitespace()).collect();
    let (w, h) = t.split_once('x').ok_or_else(|| ImageError::Invalid(format!("invalid setting: \"{text}\" is not a size like 1024x1024")))?;
    let (w, h) = (w.parse::<u32>(), h.parse::<u32>());
    match (w, h) {
        (Ok(w), Ok(h)) => check_size(w, h).map(|_| (w, h)),
        _ => Err(ImageError::Invalid(format!("invalid setting: \"{text}\" is not a size like 1024x1024"))),
    }
}

/// A file name part that is safe on every system.
pub fn safe_stem(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(60).collect();
    if t.is_empty() {
        "image".into()
    } else {
        t
    }
}

/// The provider by name.
pub fn provider(name: &str) -> Option<Box<dyn ImageProvider>> {
    match name {
        "card" => Some(Box::new(card::CardProvider::system())),
        "diffusion" => Some(Box::new(diffusion::DiffusionProvider::from_env())),
        _ => None,
    }
}

/// Where generated images are kept.
pub fn output_dir() -> PathBuf {
    std::env::var_os("AQUA_IMAGE_DIR").map(PathBuf::from).unwrap_or_else(|| crate::runtime::workspace_root().join("data").join("generated_images"))
}
