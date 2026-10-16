//! Drawing a prompt with a local image-generation service (Stable Diffusion WebUI / Forge API:
//! `POST {endpoint}/sdapi/v1/txt2img`). It is a service on this computer that the person runs; Aqua does not
//! bundle a model. The address must be loopback, so a prompt is never sent off the machine by this provider.

use super::{check_size, ImageError, ImageProvider, ImageRequest, ImageResult};
use base64::Engine;
use std::path::Path;
use std::time::Duration;

pub struct DiffusionProvider {
    endpoint: Option<String>,
    timeout: Duration,
}

impl DiffusionProvider {
    pub fn from_env() -> Self {
        Self { endpoint: std::env::var("AQUA_IMAGE_ENDPOINT").ok().map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty()), timeout: Duration::from_secs(600) }
    }

    pub fn new(endpoint: &str, timeout: Duration) -> Self {
        Self { endpoint: Some(endpoint.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty()), timeout }
    }

    fn checked_endpoint(&self) -> Result<&str, ImageError> {
        let Some(e) = self.endpoint.as_deref() else {
            return Err(ImageError::NotSetUp("no image generator is set up: run a local Stable Diffusion service and set AQUA_IMAGE_ENDPOINT (for example http://127.0.0.1:7860), or use the card provider for a text graphic".into()));
        };
        let host = crate::flows::net::http_host(e).ok_or_else(|| ImageError::NotSetUp("the image generator address must start with http:// and be on this computer".into()))?;
        if !crate::flows::net::is_local_http(e) {
            return Err(ImageError::NotSetUp(format!("the image generator address must be on this computer (127.0.0.1 or localhost), not {host}: prompts are not sent elsewhere")));
        }
        Ok(e)
    }
}

impl ImageProvider for DiffusionProvider {
    fn name(&self) -> &'static str {
        "diffusion"
    }

    fn ready(&self) -> Result<(), ImageError> {
        self.checked_endpoint().map(|_| ())
    }

    fn generate(&self, req: &ImageRequest, dir: &Path, stem: &str) -> Result<ImageResult, ImageError> {
        check_size(req.width, req.height)?;
        let endpoint = self.checked_endpoint()?.to_string();
        let prompt: String = req.prompt.trim().chars().take(1000).collect();
        if prompt.is_empty() {
            return Err(ImageError::Invalid("invalid setting: the image prompt is empty".into()));
        }
        let mut body = serde_json::json!({ "prompt": prompt, "negative_prompt": "blurry, low quality, distorted, text, watermark", "width": req.width, "height": req.height, "steps": 20, "batch_size": 1 });
        if let Some(seed) = req.seed {
            body["seed"] = serde_json::json!(seed);
        }
        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let resp = match agent.post(&format!("{endpoint}/sdapi/v1/txt2img")).send_json(body) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) if code >= 500 => return Err(ImageError::Failed { message: format!("the image generator returned an error ({code}); it may be busy or out of memory"), retryable: true }),
            Err(ureq::Error::Status(code, _)) => return Err(ImageError::Failed { message: format!("the image generator refused the request ({code}); check the size and settings"), retryable: false }),
            Err(e) => return Err(ImageError::Failed { message: format!("could not connect to the image generator at {endpoint} (connection error: {})", e.to_string().chars().take(120).collect::<String>()), retryable: true }),
        };
        let json: serde_json::Value = resp.into_json().map_err(|_| ImageError::Failed { message: "the image generator answered with something that is not JSON".into(), retryable: false })?;
        let b64 = json["images"][0].as_str().ok_or_else(|| ImageError::Failed { message: "the image generator answered without an image".into(), retryable: false })?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64.split(',').next_back().unwrap_or(b64)).map_err(|_| ImageError::Failed { message: "the image generator's answer was not a valid image".into(), retryable: false })?;
        // Trust nothing: it must decode as a real image before it is kept.
        let decoded = image::load_from_memory(&bytes).map_err(|_| ImageError::Failed { message: "the image generator's answer was not a valid image".into(), retryable: false })?;
        std::fs::create_dir_all(dir).map_err(|e| ImageError::Failed { message: format!("could not create the images folder: {e}"), retryable: false })?;
        let path = dir.join(format!("{}.png", super::safe_stem(stem)));
        decoded.save_with_format(&path, image::ImageFormat::Png).map_err(|e| ImageError::Failed { message: format!("could not save the image: {e}"), retryable: false })?;
        Ok(ImageResult { path, width: decoded.width(), height: decoded.height(), alt_text: format!("An image of: {}", prompt.chars().take(200).collect::<String>()), provider: "diffusion" })
    }
}
