//! What Aqua knows about each social platform, so nothing else has to hard-code them.
//!
//! A platform is data: its name and aliases, its home page, and what it accepts. The planner and the
//! capability resolver read this table; adding a platform is one entry. Actions come from the
//! platform's *mechanism*: with no API connector configured, everything goes through the browser.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Platform {
    pub id: &'static str,
    pub label: &'static str,
    /// Lower-case single words that name it in a request.
    pub aliases: &'static [&'static str],
    pub url: &'static str,
    /// The platform will not take a post without an image.
    pub image_required: bool,
    /// Posts here are video; a text or image post is not possible.
    pub video_only: bool,
    /// The image size the post looks best with (one of the image node's sizes).
    pub image_size: &'static str,
}

pub const PLATFORMS: &[Platform] = &[
    Platform { id: "linkedin", label: "LinkedIn", aliases: &["linkedin"], url: "https://www.linkedin.com", image_required: false, video_only: false, image_size: "1200x627" },
    Platform { id: "instagram", label: "Instagram", aliases: &["instagram", "insta", "ig"], url: "https://www.instagram.com", image_required: true, video_only: false, image_size: "1024x1024" },
    Platform { id: "x", label: "X (Twitter)", aliases: &["twitter", "tweet", "tweets"], url: "https://x.com", image_required: false, video_only: false, image_size: "1200x627" },
    Platform { id: "facebook", label: "Facebook", aliases: &["facebook", "fb"], url: "https://www.facebook.com", image_required: false, video_only: false, image_size: "1200x627" },
    Platform { id: "youtube", label: "YouTube", aliases: &["youtube"], url: "https://www.youtube.com", image_required: false, video_only: true, image_size: "1200x627" },
    Platform { id: "tiktok", label: "TikTok", aliases: &["tiktok"], url: "https://www.tiktok.com", image_required: false, video_only: true, image_size: "1024x1024" },
];

pub fn get(id: &str) -> Option<&'static Platform> {
    PLATFORMS.iter().find(|p| p.id == id)
}

/// The platforms a flow can really post to today. The table above lists more of them (their pages, what they accept) so that a flow can be drawn, checked and dry-run for any of
/// them; a real post needs a recipe for the platform's own page, and only LinkedIn has one that is on by default (`flows/social/linkedin.rs`).
/// This is the default list. Ask `publishable_ids()` for the list that is true now: a recipe that is behind a switch is added there.
pub const PUBLISHABLE: &[&str] = &["linkedin"];

/// The switch for the Facebook recipe (`flows/social/facebook.rs`). It is off unless the variable is set to `on` when Aqua starts. The recipe has only been rehearsed on a stand-in
/// page, never on Facebook itself, so the first real post is the person's: see `documentation/AI_FLOWS.md`.
pub const FACEBOOK_SWITCH: &str = "AQUA_PLATFORM_FACEBOOK";

/// Is a switch value "on"? Anything else, and a missing value, is off.
pub fn switch_is_on(value: Option<&str>) -> bool {
    matches!(value.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true" | "yes"))
}

/// Is the Facebook recipe switched on?
pub fn facebook_enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = test_switch::facebook() {
        return on;
    }
    switch_is_on(std::env::var(FACEBOOK_SWITCH).ok().as_deref())
}

/// The platforms a flow can post to now: the default list, and Facebook when its switch is on. The planner, the checker, the refusal before a run starts and the step itself all ask
/// this (through `can_publish` and `not_built_note`), so they always agree.
pub fn publishable_ids() -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = PUBLISHABLE.to_vec();
    if facebook_enabled() {
        ids.push("facebook");
    }
    ids
}

pub fn can_publish(p: &Platform) -> bool {
    publishable_ids().contains(&p.id)
}

/// What to tell the person about a platform Aqua cannot post to yet, in one clause ("posting to Facebook is not built yet (only LinkedIn can be posted to today)"); `None` when it can.
pub fn not_built_note(p: &Platform) -> Option<String> {
    let ids = publishable_ids();
    if ids.contains(&p.id) {
        return None;
    }
    let works = ids.iter().filter_map(|id| get(id)).map(|p| p.label).collect::<Vec<_>>().join(" and ");
    Some(format!("posting to {} is not built yet (only {works} can be posted to today)", p.label))
}

/// A switch for tests that is private to the thread that sets it, so that a test of the switched-on state never changes what another test running at the same moment sees
/// (the environment of the process is shared by all tests). It ends when the guard is dropped, also after a panic.
#[cfg(test)]
pub(crate) mod test_switch {
    use std::cell::Cell;

    thread_local! {
        static FACEBOOK: Cell<Option<bool>> = const { Cell::new(None) };
    }

    pub struct Guard {
        previous: Option<bool>,
    }

    /// Make `facebook_enabled()` answer `on` on this thread until the guard is dropped.
    pub fn set_facebook(on: bool) -> Guard {
        Guard { previous: FACEBOOK.with(|f| f.replace(Some(on))) }
    }

    pub fn facebook() -> Option<bool> {
        FACEBOOK.with(|f| f.get())
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            FACEBOOK.with(|f| f.set(self.previous));
        }
    }
}

/// The words of a text, split on anything that is not a letter or digit, with their position.
fn words(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((s, &text[s..i]));
        }
    }
    if let Some(s) = start {
        out.push((s, &text[s..]));
    }
    out
}

/// The platforms a text mentions, in the order they first appear, each once. A capital "X" on its own
/// (or beside "Twitter") means X; a lower-case "x" is not taken for it.
pub fn mentioned(text: &str) -> Vec<(usize, &'static Platform)> {
    let mut found: Vec<(usize, &'static Platform)> = Vec::new();
    for (pos, w) in words(text) {
        let lower = w.to_ascii_lowercase();
        let hit = PLATFORMS.iter().find(|p| p.aliases.contains(&lower.as_str())).or_else(|| if w == "X" { get("x") } else { None });
        if let Some(p) = hit {
            if !found.iter().any(|(_, f)| f.id == p.id) {
                found.push((pos, p));
            }
        }
    }
    found
}

/// The platform whose page a URL belongs to.
pub fn from_url(url: &str) -> Option<&'static Platform> {
    let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("").to_ascii_lowercase();
    PLATFORMS.iter().find(|p| {
        let h = p.url.split("://").nth(1).unwrap_or(p.url).trim_start_matches("www.");
        host.trim_start_matches("www.") == h || host.ends_with(&format!(".{h}"))
    })
}

/// The credential name a platform's account is stored under.
pub fn credential(p: &Platform) -> String {
    format!("{}/main", p.id)
}

/// What the browser can do on any platform today: it can write, attach and publish a post, and read one back.
/// Scheduling on the platform, editing and deleting a published post are not done through the browser.
pub const BROWSER_ACTIONS: &[&str] = &["create_post", "upload_media", "publish", "get_post", "read_comments", "reply_comment"];

/// Platforms that have an API connector configured, and what each offers. None yet: a connector is added
/// here (and in the credential store) when one exists, and the planner and validator pick it up.
pub fn connector_actions(_p: &Platform) -> &'static [&'static str] {
    &[]
}

/// The actions a platform can actually be asked to do, from any mechanism. Only what is supported is offered.
pub fn available_actions(p: &Platform) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = BROWSER_ACTIONS.to_vec();
    // The Facebook recipe writes, attaches, publishes and reads a post. It does not read or answer comments (that reading is written for LinkedIn's page).
    if p.id == "facebook" && can_publish(p) {
        v.retain(|a| !matches!(*a, "read_comments" | "reply_comment"));
    }
    for a in connector_actions(p) {
        if !v.contains(a) {
            v.push(a);
        }
    }
    v
}
