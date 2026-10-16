//! The "Generate image" step: picks the provider the step names, makes a real file in the images folder, and
//! hands the path (and a description for people who cannot see it) to the steps after it.

use crate::flows::image::{self, ImageError, ImageProvider, ImageRequest};
use crate::flows::model::NodeKind;
use crate::flows::run::exec::{NodeCtx, NodeError, NodeExecutor, NodeResult};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;

pub type Providers = Arc<dyn Fn(&str) -> Option<Box<dyn ImageProvider>> + Send + Sync>;

pub struct ImageNodeExecutor {
    providers: Providers,
    dir: PathBuf,
}

impl ImageNodeExecutor {
    /// The real providers, writing to the images folder.
    pub fn system() -> Arc<Self> {
        Arc::new(Self { providers: Arc::new(image::provider), dir: image::output_dir() })
    }

    pub fn new(providers: Providers, dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { providers, dir })
    }
}

fn text(cfg: &Value, key: &str) -> String {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
}

fn to_node_error(e: ImageError) -> NodeError {
    match e {
        ImageError::NotSetUp(m) | ImageError::Invalid(m) => NodeError::Fatal(m),
        ImageError::Failed { message, retryable: true } => NodeError::Retryable(message),
        ImageError::Failed { message, retryable: false } => NodeError::Fatal(message),
    }
}

impl NodeExecutor for ImageNodeExecutor {
    fn kinds(&self) -> &[NodeKind] {
        &[NodeKind::ImageGeneration]
    }

    fn execute(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
        let cfg = &ctx.config;
        let name = match text(cfg, "provider").as_str() {
            "" | "local" => "diffusion".to_string(),
            other => other.to_string(),
        };
        let provider = (self.providers)(&name).ok_or_else(|| NodeError::Fatal(format!("invalid setting: \"{name}\" is not an image provider (diffusion or card)")))?;
        let size = text(cfg, "size");
        let (width, height) = image::parse_size(if size.is_empty() { "1024x1024" } else { &size }).map_err(to_node_error)?;
        let req = ImageRequest { prompt: text(cfg, "prompt"), title: text(cfg, "title"), subtitle: text(cfg, "subtitle"), width, height, style: text(cfg, "style"), seed: cfg.get("seed").and_then(|v| v.as_u64()) };
        let stem = format!("run{}_{}", ctx.run_id, ctx.node.id);
        let r = provider.generate(&req, &self.dir, &stem).map_err(to_node_error)?;
        let alt = match text(cfg, "alt_text") {
            a if a.is_empty() => r.alt_text.clone(),
            a => a,
        };
        let path = r.path.to_string_lossy().to_string();
        let mut o = Map::new();
        o.insert("image".into(), json!(path));
        o.insert("alt_text".into(), json!(alt));
        Ok(NodeResult { outputs: o, logs: vec![format!("Made a {}x{} image with the {} provider: {path}", r.width, r.height, r.provider)] })
    }
}
