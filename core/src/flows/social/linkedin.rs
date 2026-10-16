//! LinkedIn, through the browser. Written from how the LinkedIn feed is laid out for a person using it (a
//! "Start a post" button, a post window with a text editor, "Add media", "Post", and a "Post successful"
//! notice with a "View post" link). It has been checked against a local page built the same way, not against
//! LinkedIn itself: no real account was used or posted to. LinkedIn changes its pages; when it does, the step
//! stops with "element not found" and lists what is on the page, and the alternatives below are where to add.

use super::{compose_text, link_url, Outcome, Page, SocialNodeExecutor};
use super::comments::{clean, comment_id, parse_comments, Comment, MAX_COMMENTS};
use crate::flows::browser_exec::{elements, pick, Element, Found};
use crate::flows::browser_exec::text_of;
use crate::flows::run::exec::NodeError;
use serde_json::Value;

/// Where the recipe starts. A local address may replace it (`AQUA_SOCIAL_LINKEDIN_URL`) to rehearse against a
/// test page; any other address is ignored, so this cannot point the recipe at some other site.
fn start_url() -> String {
    if let Ok(u) = std::env::var("AQUA_SOCIAL_LINKEDIN_URL") {
        if crate::flows::net::is_local_http(&u) {
            return u;
        }
    }
    "https://www.linkedin.com/feed/".into()
}

const START: &[&str] = &["button:Start a post", "button:Start a post, to share"];
const EDITOR: &[&str] = &["textbox:Text editor for creating content", "textbox:What do you want to talk about?", "textbox:Text editor"];
const ADD_MEDIA: &[&str] = &["button:Add media", "button:Add a photo", "button:Add an image"];
const MEDIA_DONE: &[&str] = &["button:Next", "button:Done"];
const POST: &[&str] = &["button:Post"];
const LOAD_MORE: &[&str] = &["button:Load more comments", "button:See more comments", "button:Show more comments"];
const REPLY_OPEN: &[&str] = &["button:Reply"];
const REPLY_SUBMIT: &[&str] = &["button:Post", "button:Post reply", "button:Submit", "button:Comment", "button:Reply"];
/// The longest comment LinkedIn takes.
pub const MAX_REPLY_CHARS: usize = 1250;
const LIVE_NOTICES: &[&str] = &["Post successful", "Your post is live", "Post published"];

/// Is the post window open (an editor and a Post button)?
fn composer_open(p: &Page) -> bool {
    p.is_on_page(EDITOR) && p.is_on_page(POST)
}

/// Open a post window. With `reuse`, one that is already open is continued; otherwise the feed is opened again,
/// which throws away anything left in a window from before.
fn open_composer(p: &Page, reuse: bool, logs: &mut Vec<String>) -> Result<(), NodeError> {
    if reuse && composer_open(p) {
        return Ok(());
    }
    let (at, _) = p.goto(&start_url())?;
    logs.push(format!("Opened {at}"));
    let start = p.find_any(START, "\"Start a post\" button")?;
    p.click(&start, "\"Start a post\"")?;
    logs.push("Opened the post window".into());
    Ok(())
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
    let add = p.find_any(ADD_MEDIA, "\"Add media\" button")?;
    p.click(&add, "\"Add media\"")?;
    p.upload(&file)?;
    if let Ok(done) = p.find_any(MEDIA_DONE, "\"Next\" button after adding media") {
        p.click(&done, "\"Next\"")?;
    }
    logs.push("Attached the image".into());
    Ok(())
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
            // Everything that can be checked without the page is checked first, so a mistake does not leave a
            // half-made post: the length, and the image (which is also copied where the browser may upload from).
            if text.chars().count() > 3000 {
                check_text(&text)?;
            }
            if !media.is_empty() {
                ex.stage_media(&media)?;
            }
            // A window is continued only when this run made it (create_post / upload_media) and this step brings no
            // text of its own. Anything else starts a fresh window, so an old draft is never the thing that is posted.
            let already = text.is_empty() && ex.has_draft(p.ctx.run_id) && composer_open(p);
            if !already {
                check_text(&text)?;
                open_composer(p, false, &mut logs)?;
                write_text(p, &text, &mut logs)?;
                if !media.is_empty() {
                    attach(ex, p, &media, &mut logs)?;
                }
            }
            let post = p.find_any(POST, "\"Post\" button")?;
            p.click(&post, "\"Post\"").map_err(|e| match e {
                // The click was sent; if the answer was lost, it may have gone out.
                NodeError::Retryable(m) => NodeError::Unknown(m),
                other => other,
            })?;
            logs.push("Pressed Post".into());
            let seen = LIVE_NOTICES.iter().any(|n| p.wait_text(n));
            if !seen {
                return Err(NodeError::Unknown("Post was pressed, but LinkedIn did not show that the post is live, so it is not known whether it was published. Check your LinkedIn feed before running this step again".into()));
            }
            let url = p.snapshot_at().ok().and_then(|(s, at)| link_url(&s, "View post").map(|u| super::absolutize(&at, &u)));
            logs.push(match &url {
                Some(u) => format!("LinkedIn confirmed the post: {u}"),
                None => "LinkedIn confirmed the post (no link was shown)".into(),
            });
            Ok(Outcome { post_url: url, status: "published: LinkedIn showed the post is live".into(), logs, extra: Default::default() })
        }
        "read_comments" => {
            let url = text_of(cfg, "post_url");
            if !is_post_url(&url) {
                return Err(NodeError::Fatal("invalid setting: read_comments needs the address of a LinkedIn post (set Post address)".into()));
            }
            let limit = cfg.get("limit").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(50).clamp(1, MAX_COMMENTS as u64) as usize;
            let me = text_of(cfg, "my_name");
            let (at, mut snap) = p.goto(&url)?;
            logs.push(format!("Opened {at}"));
            // Comments load in pages: ask for more, a few times, until there are enough or there is no more.
            for _ in 0..5 {
                let have = parse_comments(&snap, &me).len();
                if have >= limit {
                    break;
                }
                let Ok(more) = p.find_any(LOAD_MORE, "\"Load more comments\" button") else { break };
                p.click(&more, "\"Load more comments\"")?;
                snap = p.snapshot()?;
                // A click that brought nothing new means there is no more, whatever the button still says.
                if parse_comments(&snap, &me).len() <= have {
                    break;
                }
            }
            let mut all = parse_comments(&snap, &me);
            let total = all.len();
            all.truncate(limit);
            logs.push(format!("Read {} of {total} comments", all.len()));
            let mut extra = serde_json::Map::new();
            extra.insert("comments".into(), serde_json::to_value(&all).unwrap_or(serde_json::Value::Null));
            extra.insert("count".into(), serde_json::json!(all.len()));
            Ok(Outcome { post_url: Some(at), status: format!("read: {} comment{}", all.len(), if all.len() == 1 { "" } else { "s" }), logs, extra })
        }
        "reply_comment" => {
            let url = text_of(cfg, "post_url");
            let id = text_of(cfg, "comment_id");
            if id.is_empty() {
                return Err(NodeError::Fatal("invalid setting: reply_comment needs the id of the comment (Comment id)".into()));
            }
            let reply_id = reply_to(p, &url, &id, &text_of(cfg, "my_name"), &text_of(cfg, "reply_text"), &mut logs)?;
            let mut extra = serde_json::Map::new();
            extra.insert("reply_id".into(), serde_json::json!(reply_id));
            Ok(Outcome { post_url: Some(url), status: "published: the reply shows under the comment".into(), logs, extra })
        }
        "get_post" => {
            let url = text_of(cfg, "post_url");
            if !(url.starts_with("https://www.linkedin.com/") || url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost")) {
                return Err(NodeError::Fatal("invalid setting: get_post needs the address of a LinkedIn post (set Post address)".into()));
            }
            let (at, snap) = p.goto(&url)?;
            let shown: String = snap.chars().take(4000).collect();
            logs.push(format!("Read {at}"));
            Ok(Outcome { post_url: Some(at), status: format!("read: {}", shown.lines().filter(|l| !l.trim().is_empty()).take(1).next().unwrap_or("").trim().chars().take(80).collect::<String>()), logs, extra: Default::default() })
        }
        other => Err(NodeError::Fatal(format!("invalid setting: LinkedIn cannot \"{other}\" through the browser yet (create_post, upload_media, publish, get_post)"))),
    }
}


/// The first of `specs` that is in `lines` exactly once, ignoring elements in `skip` (which were already there before).
fn in_lines(lines: &[String], specs: &[&str], skip: &[String]) -> Option<Element> {
    let page: Vec<Element> = elements(&lines.join("\n")).into_iter().filter(|e| !skip.contains(&e.target)).collect();
    specs.iter().find_map(|s| match pick(s, &page) {
        Found::One(e) => Some(e),
        _ => None,
    })
}

fn targets_of(lines: &[String]) -> Vec<String> {
    elements(&lines.join("\n")).into_iter().map(|e| e.target).collect()
}

/// The comment `id`, found on the page again, loading more comments if it is not among those showing.
fn find_comment(p: &Page, id: &str, me: &str) -> Result<(Comment, Vec<Comment>), NodeError> {
    let mut snap = p.snapshot()?;
    for _ in 0..6 {
        let all = parse_comments(&snap, me);
        if let Some(c) = all.iter().find(|c| c.id == id) {
            return Ok((c.clone(), all));
        }
        let Ok(more) = p.find_any(LOAD_MORE, "\"Load more comments\" button") else { break };
        let have = all.len();
        p.click(&more, "\"Load more comments\"")?;
        snap = p.snapshot()?;
        if parse_comments(&snap, me).len() <= have {
            break;
        }
    }
    Err(NodeError::Fatal("element not found on the page: the comment is no longer there, or its words changed since the draft was written, so nothing was posted".into()))
}

/// Answer one comment under a post. Nothing is typed anywhere but in the reply box that opens under that comment, and
/// the reply only counts as posted when it shows on the page under that comment.
pub fn reply_to(p: &Page, post_url: &str, comment_id_: &str, me: &str, text: &str, logs: &mut Vec<String>) -> Result<String, NodeError> {
    if !is_post_url(post_url) {
        return Err(NodeError::Fatal("invalid setting: a reply needs the address of a LinkedIn post (set Post address)".into()));
    }
    let text = clean(text);
    if text.trim().is_empty() {
        return Err(NodeError::Fatal("invalid setting: the reply has no text".into()));
    }
    if text.chars().count() > MAX_REPLY_CHARS {
        return Err(NodeError::Fatal(format!("invalid setting: the reply is {} characters; LinkedIn takes at most {MAX_REPLY_CHARS}", text.chars().count())));
    }
    let (at, _) = p.goto(post_url)?;
    logs.push(format!("Opened {at}"));
    let (target, all) = find_comment(p, comment_id_, me)?;
    if target.is_mine {
        return Err(NodeError::Fatal("invalid setting: that is your own comment, not one to answer".into()));
    }
    if target.answered {
        return Err(NodeError::Fatal("invalid setting: you have already answered that comment, so it is not answered again".into()));
    }
    let before_ids: Vec<String> = all.iter().map(|c| c.id.clone()).collect();
    // Open the reply box under that comment: its own Reply button, not another comment's.
    let Some(open) = in_lines(&target.own_lines, REPLY_OPEN, &[]) else {
        return Err(NodeError::Fatal("element not found on the page: the Reply button under that comment".into()));
    };
    let known = targets_of(&target.all_lines);
    p.click(&open, "\"Reply\" under the comment")?;
    logs.push("Opened the reply box".into());
    // The box must have opened inside that comment. Never fall back to the post's own comment box: that would post a
    // new comment of the person's own, in public, instead of a reply.
    let (t2, _) = find_comment(p, comment_id_, me)?;
    let editors: Vec<Element> = elements(&t2.all_lines.join("\n")).into_iter().filter(|e| e.role.eq_ignore_ascii_case("textbox") && !known.contains(&e.target)).collect();
    let editor = match editors.as_slice() {
        [one] => one.clone(),
        [] => return Err(NodeError::Fatal("element not found on the page: the reply box did not open under that comment, so nothing was typed".into())),
        many => return Err(NodeError::Fatal(format!("element not found reliably: more than one box opened under that comment ({}), so nothing was typed", crate::flows::browser_exec::describe(many)))),
    };
    p.type_into(&editor, &text, "the reply box")?;
    logs.push(format!("Wrote {} characters", text.chars().count()));
    // The button that sends it is a new one that appeared with the box.
    let (t3, _) = find_comment(p, comment_id_, me)?;
    let mut skip = known.clone();
    skip.push(editor.target.clone());
    let Some(submit) = in_lines(&t3.all_lines, REPLY_SUBMIT, &skip) else {
        return Err(NodeError::Fatal("element not found on the page: the button that sends the reply, under that comment. The reply was typed but not sent".into()));
    };
    p.click(&submit, "the button that sends the reply").map_err(|e| match e {
        // The press was sent; if the answer was lost, it may have gone out.
        NodeError::Retryable(m) => NodeError::Unknown(m),
        other => other,
    })?;
    logs.push("Pressed send".into());
    // Posted means it can be seen: a new comment under that one, with those words.
    let want = comment_id(if me.is_empty() { "" } else { me }, &text);
    let snippet: String = text.chars().take(30).collect();
    let seen = p.wait_text(&snippet);
    let mut reply_id = String::new();
    if seen {
        let now = parse_comments(&p.snapshot()?, me);
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        if let Some(r) = now.iter().find(|c| c.parent.as_deref() == Some(&target.id) && !before_ids.contains(&c.id) && (c.id == want || norm(&c.text) == norm(&text) || norm(&c.text).ends_with(&norm(&text)))) {
            reply_id = r.id.clone();
        }
    }
    if reply_id.is_empty() {
        return Err(NodeError::Unknown("the send button was pressed, but the reply did not show under that comment, so it is not known whether it was posted. Check the post before trying again".into()));
    }
    logs.push("The reply shows under the comment".into());
    Ok(reply_id)
}

/// A LinkedIn post address (or the local stand-in used for rehearsal).
pub fn is_post_url(url: &str) -> bool {
    url.starts_with("https://www.linkedin.com/") || crate::flows::net::is_local_http(url)
}

fn check_text(text: &str) -> Result<(), NodeError> {
    if text.trim().is_empty() {
        return Err(NodeError::Fatal("invalid setting: the post has no text".into()));
    }
    if text.chars().count() > 3000 {
        return Err(NodeError::Fatal(format!("invalid setting: the post is {} characters; LinkedIn takes at most 3000", text.chars().count())));
    }
    Ok(())
}
