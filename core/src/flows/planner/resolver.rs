//! The capability resolver: for each step, which mechanism will really do the work.
//!
//! Connector first, browser second. A platform step goes through the platform's API connector when one
//! is set up and the browser when not; research goes through the agent and the web; text and image
//! steps use the local model. The answer is shown to the person, so nothing happens through a route
//! they were not told about.

use crate::flows::model::*;
use crate::flows::platform;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resolution {
    pub node: String,
    pub name: String,
    pub capability: String,
    /// connector | browser | agent | local_model | tool_host | http | local_files | person | internal
    pub mechanism: String,
    pub note: String,
}

pub fn resolve(def: &FlowDef) -> Vec<Resolution> {
    def.nodes
        .iter()
        .map(|n| {
            let cfg = |k: &str| n.config.get(k).and_then(|v| v.as_str()).unwrap_or("");
            let (capability, mechanism, note): (String, &str, String) = match n.kind {
                NodeKind::SocialMedia => {
                    let p = platform::get(cfg("platform"));
                    let action = cfg("action");
                    match p {
                        Some(p) if platform::connector_actions(p).contains(&action) && cfg("mode") != "browser" => (format!("{} {action}", p.label), "connector", format!("uses the {} connector", p.label)),
                        Some(p) => (
                            format!("{} {action}", p.label),
                            "browser",
                            if platform::BROWSER_ACTIONS.contains(&action) {
                                format!("no {} connector is set up, so Aqua's browser does it (you are asked before it publishes)", p.label)
                            } else {
                                format!("{} cannot \"{action}\" through the browser", p.label)
                            },
                        ),
                        None => ("post".into(), "browser", "the platform is not set".into()),
                    }
                }
                NodeKind::Website | NodeKind::Browser => ("browser".into(), "browser", "Aqua's managed browser".into()),
                NodeKind::Research => ("web research".into(), "agent", "the Aqua agent searches and reads pages".into()),
                NodeKind::AiAgent => ("agent".into(), "agent", "the Aqua agent".into()),
                NodeKind::TextGeneration => ("write text".into(), "local_model", "the local language model".into()),
                NodeKind::ImageGeneration => ("make an image".into(), "local_model", format!("the {} image provider", if cfg("provider").is_empty() { "diffusion" } else { cfg("provider") })),
                NodeKind::Tool => ("tool".into(), "tool_host", "the guarded tool host".into()),
                NodeKind::Http => ("web request".into(), "http", "an HTTP request".into()),
                NodeKind::CommentReplies => ("draft replies".into(), "local_model", "the local language model (drafts only; nothing is posted)".into()),
                NodeKind::N8n => ("n8n workflow".into(), "n8n", "a workflow in n8n on this computer".into()),
                NodeKind::File | NodeKind::Excel | NodeKind::Database => ("data".into(), "local_files", "files inside the allowed folders".into()),
                NodeKind::Approval | NodeKind::UserInput => ("ask you".into(), "person", "waits for you".into()),
                _ => ("logic".into(), "internal", String::new()),
            };
            Resolution { node: n.id.clone(), name: n.name.clone(), capability, mechanism: mechanism.into(), note }
        })
        .collect()
}
