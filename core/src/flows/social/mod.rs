//! Phase 6: posting to social platforms.
//!
//! Connector first, browser second: a platform with an API connector would use it (none is set up; a connector
//! needs an app registered with the platform and the person's authorisation, and lives in the credential
//! store). Until then everything goes through the guarded browser, as a person would do it.
//!
//! A platform is a *recipe*: the page to start from and, for each thing it can be asked to do, the controls to
//! use, by role and visible name, with alternatives for the names a platform is known to use. Nothing is a
//! stored selector or a position. LinkedIn has a recipe. Facebook has one too, but it is behind a switch (`AQUA_PLATFORM_FACEBOOK=on`, off by default) because it has only been
//! rehearsed on a stand-in page, never on Facebook. Asking any other platform says it is not built.
//!
//! What this never does:
//! - type a password, or deal with a sign-in: it stops and says to sign in in the Aqua browser;
//! - press Post without a person's approval having been given for this step (the engine's gate; the browser
//!   refuses the click otherwise);
//! - claim a post is published without seeing the platform confirm it. If Post was pressed and no confirmation
//!   showed, the step stops as "unknown" (it is in the ledger, so it is never pressed twice) for a person to check.

pub mod comments;
pub mod facebook;
pub mod linkedin;

use crate::flows::browser_exec::{cancelled, elements, failed, looks_signed_out, pick, text_of, Driver, Element, Found};
use crate::flows::model::NodeKind;
use crate::flows::run::exec::{NodeCtx, NodeError, NodeExecutor, NodeResult};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The maximum size of an image a post carries.
pub const MAX_MEDIA_BYTES: u64 = 5 * 1024 * 1024;

pub struct SocialNodeExecutor {
    driver: Arc<dyn Driver>,
    /// Where the browser is allowed to upload from.
    uploads: PathBuf,
    /// Folders a post's media may come from (a generated image, or the uploads folder itself).
    media_roots: Vec<PathBuf>,
    /// Runs that have written a post window of their own. A window found open is only ever continued by the run that
    /// opened it: one left over from an earlier run is somebody else's draft and is never published.
    drafts: std::sync::Mutex<std::collections::HashSet<i64>>,
    /// Is the Facebook recipe switched on? Read once, when the executor is made (the variable is set before Aqua starts). The planner, the checker and the refusal before a
    /// run starts read the same switch through `platform::publishable_ids()`.
    facebook: bool,
}

impl SocialNodeExecutor {
    pub fn new(driver: Arc<dyn Driver>, uploads: PathBuf, media_roots: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self { driver, uploads, media_roots, drafts: Default::default(), facebook: crate::flows::platform::facebook_enabled() })
    }
}

/// A page as the recipes see it: look for controls, act on them, read what is showing.
pub struct Page<'a> {
    pub driver: &'a dyn Driver,
    pub ctx: &'a NodeCtx<'a>,
}

impl Page<'_> {
    pub fn goto(&self, url: &str) -> Result<(String, String), NodeError> {
        let nav = self.driver.call("browser_navigate", json!({ "url": url }));
        if !nav.ok {
            return Err(failed(&nav, "open", &format!("opening {url}")));
        }
        cancelled(self.ctx)?;
        let snap = self.driver.call("browser_snapshot", json!({}));
        let text = if snap.ok { snap.text.clone() } else { String::new() };
        let at = nav.url.clone().or(snap.url.clone()).unwrap_or_else(|| url.to_string());
        if looks_signed_out(&snap, &text) {
            return Err(NodeError::Fatal(format!("not logged in at {at}: sign in there in the Aqua browser (Aqua never asks for or stores the password), then run this step again")));
        }
        let lower = text.to_lowercase();
        if ["captcha", "security check", "verify it's you", "let's do a quick security check", "checkpoint"].iter().any(|w| lower.contains(w)) || at.contains("/checkpoint") {
            return Err(NodeError::Fatal(format!("not logged in: the site is asking for a verification (captcha or a security check) at {at}. Complete it yourself in the Aqua browser, then run this step again")));
        }
        Ok((at, text))
    }

    /// The whole page as text, and the address it is at.
    pub fn snapshot_at(&self) -> Result<(String, String), NodeError> {
        let s = self.driver.call("browser_snapshot", json!({}));
        if s.ok {
            Ok((s.text, s.url.unwrap_or_default()))
        } else {
            Err(failed(&s, "read", "reading the page"))
        }
    }

    /// The whole page as text.
    pub fn snapshot(&self) -> Result<String, NodeError> {
        let s = self.driver.call("browser_snapshot", json!({}));
        if s.ok {
            Ok(s.text)
        } else {
            Err(failed(&s, "read", "reading the page"))
        }
    }

    /// The first of `specs` (role:name, in order of preference) that is on the page exactly once. The error lists
    /// what the page shows instead, so a change of the platform's layout can be seen and not guessed at.
    pub fn find_any(&self, specs: &[&str], what: &str) -> Result<Element, NodeError> {
        let mut near: Vec<Element> = Vec::new();
        for (round, tool) in ["browser_find", "browser_snapshot"].iter().enumerate() {
            for spec in specs {
                let name = spec.split_once(':').map(|(_, n)| n).unwrap_or(spec).trim();
                let args = if round == 0 { json!({ "text": name }) } else { json!({}) };
                let o = self.driver.call(tool, args);
                if !o.ok {
                    continue;
                }
                let page = elements(&o.text);
                match pick(spec, &page) {
                    Found::One(e) => return Ok(e),
                    Found::Ambiguous(list) => return Err(NodeError::Fatal(format!("element not found reliably: more than one {what} matches \"{spec}\" ({})", crate::flows::browser_exec::describe(&list)))),
                    Found::Missing(n) => {
                        if near.is_empty() {
                            near = n;
                        }
                    }
                }
            }
        }
        Err(NodeError::Fatal(format!("element not found on the page: the {what} ({}).{}", specs.join(" or "), if near.is_empty() { " The page shows nothing like it".to_string() } else { format!(" Similar elements on the page: {}", crate::flows::browser_exec::describe(&near)) })))
    }

    pub fn is_on_page(&self, specs: &[&str]) -> bool {
        let Ok(s) = self.snapshot() else { return false };
        let page = elements(&s);
        specs.iter().any(|sp| matches!(pick(sp, &page), Found::One(_)))
    }

    pub fn click(&self, e: &Element, what: &str) -> Result<(), NodeError> {
        cancelled(self.ctx)?;
        let r = self.driver.call("browser_click", json!({ "target": e.target, "element": e.name }));
        if r.ok {
            Ok(())
        } else {
            Err(failed(&r, "click", &format!("clicking {what}")))
        }
    }

    pub fn type_into(&self, e: &Element, text: &str, what: &str) -> Result<(), NodeError> {
        cancelled(self.ctx)?;
        let r = self.driver.call("browser_type", json!({ "target": e.target, "text": text, "element": e.name }));
        if r.ok {
            Ok(())
        } else {
            Err(failed(&r, "type", &format!("typing into {what}")))
        }
    }

    pub fn upload(&self, path: &Path) -> Result<(), NodeError> {
        cancelled(self.ctx)?;
        let r = self.driver.call("browser_file_upload", json!({ "paths": [path.to_string_lossy()] }));
        if r.ok {
            Ok(())
        } else {
            Err(failed(&r, "upload", "attaching the image"))
        }
    }

    /// Wait for text to appear. `true` if it did.
    pub fn wait_text(&self, text: &str) -> bool {
        self.driver.call("browser_wait_for", json!({ "text": text })).ok
    }
}

/// The address a link points at, from a snapshot: the line after `- link "name" [ref=..]` reads `- /url: ...`.
pub fn link_url(snapshot: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = snapshot.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start().trim_start_matches("- ");
        if t.starts_with("link") && t.to_lowercase().contains(&format!("\"{}\"", name.to_lowercase())) {
            for next in lines.iter().skip(i + 1).take(3) {
                if let Some(u) = next.trim_start().trim_start_matches("- ").strip_prefix("/url:") {
                    let u = u.trim().trim_matches('"');
                    if !u.is_empty() && !u.starts_with("javascript:") {
                        return Some(u.to_string());
                    }
                }
            }
        }
    }
    None
}

/// A link from a snapshot may be relative ("/posts/1"): make it a full address against the page it was on.
pub fn absolutize(base: &str, link: &str) -> String {
    if link.starts_with("http://") || link.starts_with("https://") {
        return link.to_string();
    }
    let Some((scheme, rest)) = base.split_once("://") else { return link.to_string() };
    let host = rest.split('/').next().unwrap_or("");
    if link.starts_with('/') {
        format!("{scheme}://{host}{link}")
    } else {
        let dir = base.rsplit_once('/').map(|(d, _)| d).filter(|d| d.len() > scheme.len() + 3).unwrap_or(&format!("{scheme}://{host}")).to_string();
        format!("{dir}/{link}")
    }
}

/// The text of a post: the words, then the hashtags on a line of their own, each with its `#`.
pub fn compose_text(text: &str, hashtags: &str) -> String {
    let tags: Vec<String> = hashtags
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|t| t.trim().trim_start_matches('#'))
        .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .map(|t| format!("#{t}"))
        .collect();
    let body = text.trim().to_string();
    if tags.is_empty() {
        body
    } else if body.is_empty() {
        tags.join(" ")
    } else {
        format!("{body}\n\n{}", tags.join(" "))
    }
}

impl SocialNodeExecutor {
    pub(crate) fn has_draft(&self, run_id: i64) -> bool {
        self.drafts.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&run_id)
    }

    pub(crate) fn note_draft(&self, run_id: i64) {
        self.drafts.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(run_id);
    }

    /// Check a media file and put a copy where the browser may upload from. Only a file in a folder Aqua keeps
    /// media in is accepted: a flow cannot make the browser upload an arbitrary file from the computer.
    pub fn stage_media(&self, media: &str) -> Result<PathBuf, NodeError> {
        let src = PathBuf::from(media.trim());
        let real = std::fs::canonicalize(&src).map_err(|_| NodeError::Fatal(format!("invalid setting: the image \"{media}\" does not exist")))?;
        let allowed = self.media_roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).any(|r| real.starts_with(r));
        if !allowed {
            return Err(NodeError::Fatal("invalid setting: a post can only carry an image made by a flow or placed in Aqua's uploads folder, not a file from elsewhere on the computer".into()));
        }
        let ext = real.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if !matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
            return Err(NodeError::Fatal(format!("invalid setting: the image must be a PNG or JPG, not .{ext}")));
        }
        let size = std::fs::metadata(&real).map(|m| m.len()).unwrap_or(0);
        if size == 0 || size > MAX_MEDIA_BYTES {
            return Err(NodeError::Fatal(format!("invalid setting: the image is {} bytes; a post takes 1 byte to {} MB", size, MAX_MEDIA_BYTES / 1024 / 1024)));
        }
        std::fs::create_dir_all(&self.uploads).map_err(|e| NodeError::Fatal(format!("could not prepare the uploads folder: {e}")))?;
        let dest = self.uploads.join(format!("post_{}.{ext}", crate::flows::image::safe_stem(real.file_stem().and_then(|s| s.to_str()).unwrap_or("image"))));
        std::fs::copy(&real, &dest).map_err(|e| NodeError::Fatal(format!("could not prepare the image for upload: {e}")))?;
        Ok(dest)
    }
}

impl NodeExecutor for SocialNodeExecutor {
    fn kinds(&self) -> &[NodeKind] {
        &[NodeKind::SocialMedia]
    }

    fn execute(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
        cancelled(ctx)?;
        let cfg = &ctx.config;
        let platform = text_of(cfg, "platform");
        let action = text_of(cfg, "action");
        let mode = text_of(cfg, "mode");
        if mode == "connector" {
            return Err(NodeError::Fatal("invalid setting: no connector is set up for this platform. Use \"auto\" or \"browser\".".into()));
        }
        let page = Page { driver: self.driver.as_ref(), ctx };
        let mut out = match platform.as_str() {
            "linkedin" => linkedin::run(self, &page, &action, cfg)?,
            "facebook" if self.facebook => facebook::run(self, &page, &action, cfg)?,
            other => {
                let why = crate::flows::platform::get(other).and_then(crate::flows::platform::not_built_note).unwrap_or_else(|| format!("posting to {other} is not built yet"));
                return Err(NodeError::Fatal(format!("invalid setting: {why}. Nothing was done.")));
            }
        };
        let mut o = Map::new();
        o.insert("post_url".into(), json!(out.post_url.take().unwrap_or_default()));
        o.insert("status".into(), json!(out.status));
        for (k, v) in std::mem::take(&mut out.extra) {
            o.insert(k, v);
        }
        Ok(NodeResult { outputs: o, logs: out.logs })
    }
}

pub struct Outcome {
    pub post_url: Option<String>,
    pub status: String,
    pub logs: Vec<String>,
    /// More values the step hands on (for example `comments`).
    pub extra: Map<String, serde_json::Value>,
}

/// Answer one comment because a person approved it in Aqua (the comment inbox), outside any flow. The person's approval
/// is what lets the browser press send; it covers only this reply. Returns the reply's id as read back from the page.
pub fn post_reply(driver: &dyn Driver, post_url: &str, comment_id: &str, my_name: &str, text: &str) -> Result<String, NodeError> {
    let node: crate::flows::model::Node = serde_json::from_value(json!({ "id": "reply", "kind": "social_media", "name": "Reply" })).map_err(|e| NodeError::Fatal(e.to_string()))?;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let ctx = NodeCtx { run_id: 0, node: &node, config: Value::Null, cancel: &cancel, timezone: "" };
    let page = Page { driver, ctx: &ctx };
    let _approved = aqua_mcp_adapters::browser::pre_approved();
    let mut logs = Vec::new();
    linkedin::reply_to(&page, post_url, comment_id, my_name, text, &mut logs)
}
