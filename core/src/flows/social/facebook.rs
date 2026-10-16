//! Facebook, through the browser. It is OFF unless `AQUA_PLATFORM_FACEBOOK=on` (see `platform::publishable_ids`).
//!
//! **Read this first.** It was written from how Facebook's home page is laid out for a person using it (a "What's on your mind" box that opens a "Create post" window with a text
//! box, a "Photo/video" button and a "Post" button, and a short notice that the post is shared). It has been rehearsed only against a fake page in the tests and a stand-in page
//! built the same way (`apps/aqua-desktop/e2e/standins/facebook-standin.html`). No Facebook account was used and nothing was posted to Facebook. Every name below marked
//! `[unverified]` is from memory of the page and has not been seen on the real one. Facebook changes its pages, and puts up sign-in forms, security checks and blocks; when a
//! control is not found the step stops, says what the page shows instead, and does nothing else. The names to try are the constants below.
//!
//! The safety rules are LinkedIn's (`linkedin.rs`), and they are the ones that matter:
//! - Elements are found by accessible role and name from the page's snapshot, never by position or a selector.
//! - The Post button is pressed only when its name is exactly "Post" (a button named "Posting..." or "Post settings" is not it), and only after the person approved the step (the
//!   engine's gate; the browser refuses the click otherwise).
//! - A post is called published only when the page shows it: the post window has closed AND Facebook showed a notice that the post is shared, or the post's own first words are in
//!   the feed more often than before the window was opened. A press with no such proof is "unknown" (it is in the ledger, so Post is never pressed twice by itself).
//! - A sign-in form, a captcha or a check stops the step with "not logged in ... do it yourself in the Aqua browser". Aqua never types a password.
//! - The post's words are not written to the run log, and an image comes only from the folders a flow may attach from.

use super::{absolutize, compose_text, link_url, Outcome, Page, SocialNodeExecutor};
use crate::flows::browser_exec::{elements, pick, text_of, Element, Found};
use crate::flows::run::exec::NodeError;
use serde_json::Value;

/// Where the recipe starts. A local address may replace it (`AQUA_SOCIAL_FACEBOOK_URL`) to rehearse against a stand-in page; any other address is ignored, so this cannot point the
/// recipe at some other site.
fn start_url() -> String {
    start_url_from(std::env::var("AQUA_SOCIAL_FACEBOOK_URL").ok().as_deref())
}

fn start_url_from(configured: Option<&str>) -> String {
    match configured {
        Some(u) if crate::flows::net::is_local_http(u) => u.to_string(),
        _ => "https://www.facebook.com/".into(),
    }
}

/// The most characters a Facebook post takes. 63,206 is the figure I know from memory; it is `[unverified]` here. Posting more than a person would is refused up front.
pub const MAX_POST_CHARS: usize = 63_206;

// Names, each `[unverified]`. A name matches an element whose name is that or contains it; "What's" is written twice because the page may use the curly apostrophe.
const START: &[&str] = &["button:What's on your mind", "button:What\u{2019}s on your mind", "button:Create a post"];
const EDITOR: &[&str] = &["textbox:What's on your mind", "textbox:What\u{2019}s on your mind", "textbox:Create a public post", "textbox:Write something"];
const PHOTO: &[&str] = &["button:Photo/video"];
/// The second control some versions of the page show after "Photo/video", which opens the file chooser. When the page does not show it, the first click is taken to have opened the chooser.
const ADD_PHOTOS: &[&str] = &["button:Add photos/videos", "button:Add photos"];
const POST: &[&str] = &["button:Post"];
/// The notice that the post is shared. Looked for as text on the page.
const LIVE_NOTICES: &[&str] = &["Your post is now shared", "Your post is shared", "Your post has been shared"];
/// The link to the new post, when the notice has one.
const VIEW_LINKS: &[&str] = &["View post", "View"];
/// What the audience button of the post window is called (it says who will see the post, for example "Sharing with Only me"). It is read for the log and never changed.
const AUDIENCE_WORDS: &[&str] = &["sharing with", "edit privacy", "audience"];
/// Words of the pages Facebook shows instead of the feed when it wants something from the person. Looked for only when the "What's on your mind" box is not on the page.
const WALLS: &[&str] = &[
    "confirm your identity",
    "confirm it's you",
    "confirm it\u{2019}s you",
    "enter the code",
    "security code",
    "check your notifications on another device",
    "approve your login",
    "your account has been locked",
    "your account has been disabled",
    "temporarily blocked",
    "we suspended your account",
];

fn has(snapshot: &str, specs: &[&str]) -> bool {
    let page = elements(snapshot);
    specs.iter().any(|s| matches!(pick(s, &page), Found::One(_)))
}

/// Is the post window open (a text box and a Post button)?
fn composer_open(p: &Page) -> bool {
    p.is_on_page(EDITOR) && p.is_on_page(POST)
}

/// The page that is asking the person for something, named by its words.
fn wall_in(page_text: &str) -> Option<&'static str> {
    let lower = page_text.to_lowercase();
    WALLS.iter().find(|w| lower.contains(*w)).copied()
}

/// Open a post window. With `reuse`, one that is already open is continued; otherwise the home page is opened again, which throws away anything left in a window from before.
/// Returns the text of the home page as it was before the window opened (for counting the post in the feed afterwards), or `None` when an open window was continued.
fn open_composer(p: &Page, reuse: bool, logs: &mut Vec<String>) -> Result<Option<String>, NodeError> {
    if reuse && composer_open(p) {
        return Ok(None);
    }
    let (at, before) = p.goto(&start_url())?;
    logs.push(format!("Opened {at}"));
    let start = match p.find_any(START, "\"What's on your mind\" box") {
        Ok(e) => e,
        Err(e) => {
            // Not finding the box on a page that is asking for the person is a sign-in problem, not a layout change: say so, so that nothing tries to repair a button.
            if let Some(w) = wall_in(&before) {
                return Err(NodeError::Fatal(format!("not logged in: Facebook is asking for a check (\"{w}\") at {at}. Complete it yourself in the Aqua browser, then run this step again")));
            }
            return Err(e);
        }
    };
    p.click(&start, "the \"What's on your mind\" box")?;
    logs.push("Opened the post window".into());
    Ok(Some(before))
}

fn write_text(p: &Page, text: &str, logs: &mut Vec<String>) -> Result<(), NodeError> {
    let editor = p.find_any(EDITOR, "post text box")?;
    p.type_into(&editor, text, "the post text box")?;
    // The text itself is not logged: it is the person's post and is in the run's own record.
    logs.push(format!("Wrote {} characters", text.chars().count()));
    Ok(())
}

fn attach(ex: &SocialNodeExecutor, p: &Page, media: &str, logs: &mut Vec<String>) -> Result<(), NodeError> {
    let file = ex.stage_media(media)?;
    let add = p.find_any(PHOTO, "\"Photo/video\" button")?;
    p.click(&add, "\"Photo/video\"")?;
    if let Ok(more) = p.find_any(ADD_PHOTOS, "\"Add photos/videos\" button") {
        p.click(&more, "\"Add photos/videos\"")?;
    }
    p.upload(&file)?;
    logs.push("Attached the image".into());
    Ok(())
}

/// What the post window says about who will see the post, as a line for the log. Read only: Aqua never changes the audience, so the first post of a new account is best made with
/// the audience set to "Only me" by the person before they approve.
fn audience_line(p: &Page) -> Option<String> {
    let snap = p.snapshot().ok()?;
    let e = elements(&snap).into_iter().find(|e| e.role.eq_ignore_ascii_case("button") && AUDIENCE_WORDS.iter().any(|w| e.name.to_lowercase().contains(w)))?;
    Some(format!("The post window says: {}", e.name.chars().take(80).collect::<String>().trim()))
}

/// The Post button. Its name must be exactly "Post": a name that only contains it ("Posting...", "Post settings") is not the button that publishes.
fn post_button(p: &Page) -> Result<Element, NodeError> {
    let e = p.find_any(POST, "\"Post\" button")?;
    if !e.name.trim().eq_ignore_ascii_case("post") {
        return Err(NodeError::Fatal(format!("element not found on the page: the \"Post\" button (the nearest button is \"{}\"). Nothing was pressed", e.name)));
    }
    Ok(e)
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The first words of the post as they would show in the feed. A post that starts with too little is not looked for in the feed: a short phrase could match anything there.
fn snippet(text: &str) -> Option<String> {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let s: String = norm(first).chars().take(40).collect();
    (s.chars().count() >= 12).then_some(s)
}

/// How many lines of the page show the snippet, not counting a text box (what is typed in a box is a draft, not a post).
fn shown_count(page_text: &str, snippet: &str) -> usize {
    page_text.lines().filter(|l| !l.trim_start().trim_start_matches("- ").starts_with("textbox") && norm(l).contains(snippet)).count()
}

pub fn run(ex: &SocialNodeExecutor, p: &Page, action: &str, cfg: &Value) -> Result<Outcome, NodeError> {
    let mut logs = Vec::new();
    match action {
        "create_post" => {
            let text = compose_text(&text_of(cfg, "text"), &text_of(cfg, "hashtags"));
            check_text(&text)?;
            open_composer(p, false, &mut logs)?;
            write_text(p, &text, &mut logs)?;
            ex.note_draft(p.ctx.run_id);
            Ok(Outcome { post_url: None, status: "drafted: the post is written in the window and not published".into(), logs, extra: Default::default() })
        }
        "upload_media" => {
            let media = text_of(cfg, "media");
            if media.is_empty() {
                return Err(NodeError::Fatal("invalid setting: there is no image to attach (set Media)".into()));
            }
            open_composer(p, ex.has_draft(p.ctx.run_id), &mut logs)?;
            ex.note_draft(p.ctx.run_id);
            attach(ex, p, &media, &mut logs)?;
            Ok(Outcome { post_url: None, status: "attached: the image is in the post window and not published".into(), logs, extra: Default::default() })
        }
        "publish" => {
            let text = compose_text(&text_of(cfg, "text"), &text_of(cfg, "hashtags"));
            let media = text_of(cfg, "media");
            // Everything that can be checked without the page is checked first, so a mistake does not leave a half-made post: the length, and the image (which is also copied
            // where the browser may upload from).
            if text.chars().count() > MAX_POST_CHARS {
                check_text(&text)?;
            }
            if !media.is_empty() {
                ex.stage_media(&media)?;
            }
            // A window is continued only when this run made it (create_post / upload_media) and this step brings no text of its own. Anything else starts a fresh window, so an
            // old draft is never the thing that is posted.
            let already = text.is_empty() && ex.has_draft(p.ctx.run_id) && composer_open(p);
            let mut home_before: Option<String> = None;
            if !already {
                check_text(&text)?;
                home_before = open_composer(p, false, &mut logs)?;
                write_text(p, &text, &mut logs)?;
                if !media.is_empty() {
                    attach(ex, p, &media, &mut logs)?;
                }
            }
            if let Some(line) = audience_line(p) {
                logs.push(line);
            }
            // The post's first words in the feed before the window was opened (an earlier post with the same words must not count as this one). Only known when this step opened it.
            let looked_for = snippet(&text);
            let count_before = match (&looked_for, &home_before) {
                (Some(s), Some(h)) => Some(shown_count(h, s)),
                _ => None,
            };
            let post = post_button(p)?;
            p.click(&post, "\"Post\"").map_err(|e| match e {
                // The click was sent; if the answer was lost, it may have gone out.
                NodeError::Retryable(m) => NodeError::Unknown(m),
                other => other,
            })?;
            logs.push("Pressed Post".into());
            let notice = LIVE_NOTICES.iter().any(|n| p.wait_text(n));
            let Ok((page, at)) = p.snapshot_at() else {
                return Err(NodeError::Unknown("Post was pressed, but the page could not be read afterwards, so it is not known whether it was published. Check your Facebook profile before running this step again".into()));
            };
            // A notice with the post window still open is not a post: the window is where a rejected or unfinished post stays.
            let closed = !(has(&page, EDITOR) && has(&page, POST));
            let in_feed = closed && matches!((&looked_for, count_before), (Some(s), Some(n)) if shown_count(&page, s) > n);
            if !closed {
                return Err(NodeError::Unknown("Post was pressed, but the post window is still open on Facebook, so it is not known whether it was published. Check your Facebook profile before running this step again".into()));
            }
            let status = if notice {
                "published: Facebook showed the post is shared"
            } else if in_feed {
                "published: the post shows in the Facebook feed"
            } else {
                return Err(NodeError::Unknown("Post was pressed, but Facebook did not show that the post is shared, so it is not known whether it was published. Check your Facebook profile before running this step again".into()));
            };
            let url = VIEW_LINKS.iter().find_map(|name| link_url(&page, name)).map(|u| absolutize(&at, &u));
            logs.push(match &url {
                Some(u) => format!("Facebook confirmed the post: {u}"),
                None => "Facebook confirmed the post (no link was shown)".into(),
            });
            Ok(Outcome { post_url: url, status: status.into(), logs, extra: Default::default() })
        }
        "get_post" => {
            let url = text_of(cfg, "post_url");
            if !is_post_url(&url) {
                return Err(NodeError::Fatal("invalid setting: get_post needs the address of a Facebook post (set Post address)".into()));
            }
            let (at, snap) = p.goto(&url)?;
            let shown: String = snap.chars().take(4000).collect();
            logs.push(format!("Read {at}"));
            Ok(Outcome { post_url: Some(at), status: format!("read: {}", shown.lines().filter(|l| !l.trim().is_empty()).take(1).next().unwrap_or("").trim().chars().take(80).collect::<String>()), logs, extra: Default::default() })
        }
        other => Err(NodeError::Fatal(format!("invalid setting: Facebook cannot \"{other}\" through the browser yet (create_post, upload_media, publish, get_post)"))),
    }
}

/// A Facebook post address (or the local stand-in used for rehearsal).
pub fn is_post_url(url: &str) -> bool {
    url.starts_with("https://www.facebook.com/") || crate::flows::net::is_local_http(url)
}

fn check_text(text: &str) -> Result<(), NodeError> {
    if text.trim().is_empty() {
        return Err(NodeError::Fatal("invalid setting: the post has no text".into()));
    }
    if text.chars().count() > MAX_POST_CHARS {
        return Err(NodeError::Fatal(format!("invalid setting: the post is {} characters; Facebook takes at most {MAX_POST_CHARS}", text.chars().count())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_address_is_facebook_unless_a_local_one_is_given_for_a_rehearsal() {
        assert_eq!(start_url_from(None), "https://www.facebook.com/");
        assert_eq!(start_url_from(Some("http://127.0.0.1:8792/facebook-standin.html")), "http://127.0.0.1:8792/facebook-standin.html");
        for bad in ["https://evil.example/", "http://localhost.evil.com/", "http://127.0.0.1@evil.com/", "", "not an address"] {
            assert_eq!(start_url_from(Some(bad)), "https://www.facebook.com/", "{bad} must not become the start address");
        }
    }

    #[test]
    fn the_first_words_of_a_post_are_looked_for_in_the_feed_only_when_there_are_enough_of_them() {
        assert_eq!(snippet("Hi"), None);
        assert_eq!(snippet("\n\n   "), None);
        assert_eq!(snippet("  Five ways agents   SAVE time\n\n#ai").as_deref(), Some("five ways agents save time"));
        assert_eq!(snippet(&"word ".repeat(30)).unwrap().chars().count(), 40);
        let page = "- textbox \"What's on your mind\" [ref=e1]: Five ways agents save time\n- paragraph [ref=e2]: Five  ways agents save TIME today\n- paragraph [ref=e3]: something else";
        assert_eq!(shown_count(page, "five ways agents save time"), 1, "a draft in a text box is not a post in the feed");
    }

    #[test]
    fn only_a_page_that_asks_for_something_is_taken_for_a_wall() {
        assert_eq!(wall_in("- heading \"Confirm your identity\" [ref=e1]"), Some("confirm your identity"));
        assert_eq!(wall_in("- heading \"Your account has been locked\""), Some("your account has been locked"));
        assert_eq!(wall_in("- heading \"Feed\"\n- button \"What's on your mind, Asha?\""), None);
    }
}
