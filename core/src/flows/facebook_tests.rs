//! The Facebook recipe (`social/facebook.rs`) run against a FAKE page: a Facebook-shaped home page kept in memory. It is written from how the recipe expects the page to look. It is
//! not Facebook: no account was used, no real page was seen, and the names it shows are the names the recipe looks for. What these tests prove is the recipe's behaviour (what it
//! presses, when it stops, what it will not claim); they cannot prove that the real page looks like this.
//!
//! The switch (`AQUA_PLATFORM_FACEBOOK`) is set with `platform::test_switch`, which is private to the test's own thread: the environment of the process is shared by every test
//! that runs at the same moment, and other tests expect Facebook to be refused.

use super::browser_exec::Driver;
use super::model::*;
use super::planner::{plan, Answers, Proposal};
use super::platform::{self, test_switch};
use super::run::exec::*;
use super::social::*;
use super::supervisor::{classify, ErrorClass};
use super::validate::validate;
use aqua_mcp_adapters::browser::{BrowserErrorCode, BrowserObservation};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Home,
    Composer,
    Posted,
}

struct FakeFacebook {
    state: Mutex<State>,
    typed: Mutex<String>,
    attached: Mutex<Option<String>>,
    /// "Photo/video" was pressed and the page now shows the drop area with its own "Add photos/videos" button.
    photo_area: Mutex<bool>,
    /// A file chooser is waiting for a file.
    chooser: Mutex<bool>,
    notice_visible: Mutex<bool>,
    /// The text of the post that is now in the feed (a post stays there across page loads).
    in_feed: Mutex<Option<String>>,
    calls: Mutex<Vec<(String, Value)>>,
    /// For each click: its name, and was the browser told a person had approved this step?
    approved_clicks: Mutex<Vec<(String, bool)>>,
    epoch: Mutex<u32>,
    /// The "What's on your mind" box is named this instead.
    start_name: Mutex<Option<String>>,
    /// The post button is named this instead.
    post_name: Mutex<String>,
    /// Pressing Post fails with this browser error.
    post_error: Mutex<Option<BrowserErrorCode>>,
    signed_out: bool,
    security_check: bool,
    /// The page is a heading with these words and nothing else (an identity check, a lock).
    wall: Option<&'static str>,
    show_notice: bool,
    notice_text: &'static str,
    show_in_feed: bool,
    /// After Post the window stays open.
    stay_open: bool,
    /// "Photo/video" opens the file chooser at once (no second button).
    one_step_photo: bool,
    /// The page writes "What's" with the curly apostrophe.
    curly: bool,
    /// A post already in the feed before the test starts.
    old_post: Option<&'static str>,
    audience: Option<&'static str>,
}

impl FakeFacebook {
    fn base() -> Self {
        Self {
            state: Mutex::new(State::Home),
            typed: Mutex::new(String::new()),
            attached: Mutex::new(None),
            photo_area: Mutex::new(false),
            chooser: Mutex::new(false),
            notice_visible: Mutex::new(false),
            in_feed: Mutex::new(None),
            calls: Mutex::new(vec![]),
            approved_clicks: Mutex::new(vec![]),
            epoch: Mutex::new(0),
            start_name: Mutex::new(None),
            post_name: Mutex::new("Post".into()),
            post_error: Mutex::new(None),
            signed_out: false,
            security_check: false,
            wall: None,
            show_notice: true,
            notice_text: "Your post is now shared.",
            show_in_feed: true,
            stay_open: false,
            one_step_photo: false,
            curly: false,
            old_post: None,
            audience: Some("Edit privacy. Sharing with Only me."),
        }
    }
    fn new() -> Arc<Self> {
        Arc::new(Self::base())
    }
    fn with(f: impl FnOnce(&mut FakeFacebook)) -> Arc<Self> {
        let mut x = Self::base();
        f(&mut x);
        Arc::new(x)
    }
    fn count(&self, tool: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|c| c.0 == tool).count()
    }
    fn clicked(&self, name: &str) -> usize {
        self.approved_clicks.lock().unwrap().iter().filter(|c| c.0 == name).count()
    }
    fn mind(&self) -> String {
        format!("What{}s on your mind, Asha?", if self.curly { '\u{2019}' } else { '\'' })
    }
    fn start(&self) -> String {
        self.start_name.lock().unwrap().clone().unwrap_or_else(|| self.mind())
    }
    fn snapshot(&self) -> String {
        let e = *self.epoch.lock().unwrap();
        let r = |n: u32| format!("e{}", e * 100 + n);
        if self.signed_out {
            return format!("- heading \"Log in to Facebook\" [ref={}]\n- textbox \"Email or phone number\" [ref={}]\n- textbox \"Password\" [ref={}]\n- button \"Log in\" [ref={}]\n", r(1), r(2), r(3), r(4));
        }
        if self.security_check {
            return format!("- heading \"Let's do a quick security check\" [ref={}]\n- button \"Continue\" [ref={}]\n", r(1), r(2));
        }
        if let Some(w) = self.wall {
            return format!("- heading \"{w}\" [ref={}]\n- button \"Continue\" [ref={}]\n", r(1), r(2));
        }
        let mut s = format!("- link \"Facebook\" [ref={}]:\n  - /url: /\n", r(1));
        if *self.notice_visible.lock().unwrap() {
            s.push_str(&format!("- status [ref={}]: {}\n  - link \"View\" [ref={}]:\n    - /url: /posts/12345\n", r(8), self.notice_text, r(9)));
        }
        if *self.state.lock().unwrap() == State::Composer {
            // The window hides the feed behind it, as a modal window does.
            s.push_str(&format!("- dialog \"Create post\" [ref={}]:\n  - heading \"Create post\" [ref={}]\n", r(3), r(31)));
            if let Some(a) = self.audience {
                s.push_str(&format!("  - button \"{a}\" [ref={}]\n", r(32)));
            }
            s.push_str(&format!("  - textbox \"{}\" [ref={}]: {}\n", self.mind(), r(4), self.typed.lock().unwrap()));
            s.push_str(&format!("  - button \"Photo/video\" [ref={}]\n", r(5)));
            if *self.photo_area.lock().unwrap() {
                s.push_str(&format!("  - button \"Add photos/videos\" [ref={}]\n", r(6)));
            }
            if self.attached.lock().unwrap().is_some() {
                s.push_str(&format!("  - img \"Photo preview\" [ref={}]\n", r(7)));
            }
            s.push_str(&format!("  - button \"{}\" [ref={}]\n", self.post_name.lock().unwrap(), r(10)));
            return s;
        }
        s.push_str(&format!("- button \"{}\" [ref={}]\n- feed [ref=e300]:\n", self.start(), r(2)));
        if let Some(old) = self.old_post {
            s.push_str(&format!("  - article \"Post by Ravi Kumar\" [ref=e301]:\n    - paragraph [ref=e302]: {old}\n"));
        }
        if let Some(mine) = self.in_feed.lock().unwrap().as_ref() {
            s.push_str(&format!("  - article \"Post by Asha Rao\" [ref=e303]:\n    - paragraph [ref=e304]: {mine}\n"));
        }
        s
    }
}

fn ok(tool: &str, text: &str, url: &str) -> BrowserObservation {
    BrowserObservation { ok: true, tool: tool.into(), backend: "managed".into(), url: Some(url.into()), title: Some("Facebook".into()), text: text.into(), data: None, truncated: false, artifacts: vec![], confirmation: None, duration_ms: 1, error: None }
}

impl Driver for FakeFacebook {
    fn call(&self, tool: &str, args: Value) -> BrowserObservation {
        self.calls.lock().unwrap().push((tool.to_string(), args.clone()));
        *self.epoch.lock().unwrap() += 1;
        let url = "https://www.facebook.com/";
        let snap = self.snapshot();
        match tool {
            "browser_navigate" => {
                // A real page load: whatever a window held is gone. A post that was made stays in the feed.
                *self.state.lock().unwrap() = State::Home;
                self.typed.lock().unwrap().clear();
                *self.attached.lock().unwrap() = None;
                *self.photo_area.lock().unwrap() = false;
                *self.chooser.lock().unwrap() = false;
                *self.notice_visible.lock().unwrap() = false;
                ok(tool, "", args["url"].as_str().unwrap_or(url))
            }
            "browser_snapshot" => ok(tool, &snap, url),
            "browser_find" => {
                let q = args["text"].as_str().unwrap_or("").to_lowercase();
                let kept: String = snap.lines().filter(|l| l.to_lowercase().contains(&q)).collect::<Vec<_>>().join("\n");
                ok(tool, &kept, url)
            }
            "browser_click" => {
                let name = args["element"].as_str().unwrap_or("").to_string();
                self.approved_clicks.lock().unwrap().push((name.clone(), aqua_mcp_adapters::browser::is_pre_approved()));
                let now = *self.state.lock().unwrap();
                let post_name = self.post_name.lock().unwrap().clone();
                if now != State::Composer && name == self.start() {
                    *self.state.lock().unwrap() = State::Composer;
                } else if now == State::Composer && name == "Photo/video" {
                    if self.one_step_photo {
                        *self.chooser.lock().unwrap() = true;
                    } else {
                        *self.photo_area.lock().unwrap() = true;
                    }
                } else if now == State::Composer && name == "Add photos/videos" {
                    *self.chooser.lock().unwrap() = true;
                } else if now == State::Composer && name == post_name {
                    let failure = self.post_error.lock().unwrap().take();
                    if let Some(code) = failure {
                        return BrowserObservation::failure(tool, "managed", code, "simulated");
                    }
                    let typed = self.typed.lock().unwrap().clone();
                    if self.show_notice {
                        *self.notice_visible.lock().unwrap() = true;
                    }
                    if self.show_in_feed {
                        *self.in_feed.lock().unwrap() = Some(typed);
                    }
                    if !self.stay_open {
                        *self.state.lock().unwrap() = State::Posted;
                    }
                }
                ok(tool, "clicked", url)
            }
            "browser_type" => {
                *self.typed.lock().unwrap() = args["text"].as_str().unwrap_or("").to_string();
                ok(tool, "typed", url)
            }
            "browser_file_upload" => {
                let open = *self.chooser.lock().unwrap();
                if open {
                    *self.attached.lock().unwrap() = args["paths"][0].as_str().map(String::from);
                    *self.chooser.lock().unwrap() = false;
                    ok(tool, "uploaded", url)
                } else {
                    BrowserObservation::failure(tool, "managed", BrowserErrorCode::InvalidArguments, "no file chooser is open")
                }
            }
            "browser_wait_for" => {
                let want = args["text"].as_str().unwrap_or("").to_lowercase();
                if *self.notice_visible.lock().unwrap() && !want.is_empty() && self.notice_text.to_lowercase().contains(&want) {
                    ok(tool, "seen", url)
                } else {
                    BrowserObservation::failure(tool, "managed", BrowserErrorCode::Timeout, "text did not appear")
                }
            }
            _ => ok(tool, "", url),
        }
    }
}

fn workspace(name: &str) -> (PathBuf, PathBuf) {
    let d = std::env::temp_dir().join(format!("aqua_facebook_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let uploads = d.join("uploads");
    let images = d.join("images");
    std::fs::create_dir_all(&images).unwrap();
    (uploads, images)
}

fn png(dir: &std::path::Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    image::RgbImage::from_pixel(64, 48, image::Rgb([10, 120, 200])).save(&p).unwrap();
    p
}

/// An executor over the fake page. It reads the switch when it is made, so make it after the switch is set.
fn exec(feed: &Arc<FakeFacebook>, name: &str) -> (Arc<SocialNodeExecutor>, PathBuf, PathBuf) {
    let (uploads, images) = workspace(name);
    (SocialNodeExecutor::new(feed.clone(), uploads.clone(), vec![uploads.clone(), images.clone()]), uploads, images)
}

fn run(ex: &Arc<SocialNodeExecutor>, cfg: Value) -> Result<NodeResult, NodeError> {
    let node: Node = serde_json::from_value(json!({ "id": "post", "kind": "social_media", "name": "Post it", "config": cfg })).unwrap();
    let cancel = AtomicBool::new(false);
    ex.execute(&NodeCtx { run_id: 1, node: &node, config: node.config.clone(), cancel: &cancel, timezone: "" })
}

/// How many times the browser was made to do something (looking at the page does not count).
fn acted(feed: &FakeFacebook) -> usize {
    feed.calls.lock().unwrap().iter().filter(|c| matches!(c.0.as_str(), "browser_navigate" | "browser_click" | "browser_type" | "browser_file_upload")).count()
}

fn fb(action: &str, extra: Value) -> Value {
    let mut v = json!({ "platform": "facebook", "action": action });
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    v
}

fn on() -> test_switch::Guard {
    test_switch::set_facebook(true)
}

// ---- publishing -----------------------------------------------------------------------------------------------------------------------

#[test]
fn a_post_is_written_pressed_and_only_called_published_once_the_window_has_closed_and_facebook_says_so() {
    let _on = on();
    let feed = FakeFacebook::new();
    let (ex, _, _) = exec(&feed, "publish");
    let r = run(&ex, fb("publish", json!({ "text": "Five ways agents save time", "hashtags": "ai agents" }))).unwrap();
    assert_eq!(*feed.typed.lock().unwrap(), "Five ways agents save time\n\n#ai #agents");
    assert_eq!(r.outputs["post_url"], "https://www.facebook.com/posts/12345", "the link is made whole from what the page showed");
    assert_eq!(r.outputs["status"], "published: Facebook showed the post is shared");
    assert_eq!(*feed.state.lock().unwrap(), State::Posted);
    let tools: Vec<String> = feed.calls.lock().unwrap().iter().map(|c| c.0.clone()).collect();
    let pos = |t: &str| tools.iter().position(|x| x == t).unwrap();
    assert!(pos("browser_navigate") < pos("browser_type") && pos("browser_type") < pos("browser_wait_for"), "{tools:?}");
    assert_eq!(feed.clicked("Post"), 1, "Post was pressed once");
    assert!(r.logs.iter().all(|l| !l.contains("Five ways")), "the post's words are not copied into the log: {:?}", r.logs);
    assert!(r.logs.iter().any(|l| l == "The post window says: Edit privacy. Sharing with Only me."), "the audience the window showed is in the log: {:?}", r.logs);
}

#[test]
fn the_other_spellings_of_the_notice_and_of_the_apostrophe_are_found_and_every_one_is_tried() {
    let _on = on();
    // The notice may be worded differently; each known wording is tried in turn.
    let feed = FakeFacebook::with(|f| f.notice_text = "Your post has been shared.");
    let (ex, _, _) = exec(&feed, "wording");
    let r = run(&ex, fb("publish", json!({ "text": "A post with the third wording" }))).unwrap();
    assert!(r.outputs["status"].as_str().unwrap().starts_with("published"));
    assert_eq!(feed.count("browser_wait_for"), 3, "two wordings were tried and missed before the third showed");
    // The page may write "What's" with the curly apostrophe, for the box and for the text field.
    let feed = FakeFacebook::with(|f| f.curly = true);
    let (ex, _, _) = exec(&feed, "curly");
    let r = run(&ex, fb("publish", json!({ "text": "A post on a page with curly apostrophes" }))).unwrap();
    assert!(r.outputs["status"].as_str().unwrap().starts_with("published"));
    assert_eq!(*feed.typed.lock().unwrap(), "A post on a page with curly apostrophes");
}

#[test]
fn a_post_with_an_image_attaches_a_copy_from_the_uploads_folder_with_one_button_or_two() {
    let _on = on();
    for (label, one_step, clicks) in [("two", false, 4), ("one", true, 3)] {
        let feed = FakeFacebook::with(|f| f.one_step_photo = one_step);
        let (ex, uploads, images) = exec(&feed, &format!("media_{label}"));
        let img = png(&images, "cover.png");
        let r = run(&ex, fb("publish", json!({ "text": "With a picture", "media": img.to_string_lossy() }))).unwrap();
        assert!(r.outputs["status"].as_str().unwrap().starts_with("published"), "{label}");
        let sent = feed.attached.lock().unwrap().clone().expect("an image was attached");
        assert!(sent.starts_with(&*uploads.to_string_lossy()) && sent.ends_with(".png"), "the browser is given the copy in the uploads folder: {sent}");
        assert!(std::path::Path::new(&sent).exists());
        assert_eq!(feed.count("browser_file_upload"), 1, "{label}");
        // The box, "Photo/video", (the second button, when the page has one) and Post.
        assert_eq!(feed.count("browser_click"), clicks, "{label}: {:?}", feed.approved_clicks.lock().unwrap());
    }
}

#[test]
fn only_an_image_aqua_made_or_was_given_can_be_attached_and_a_bad_one_stops_before_anything_is_typed() {
    let _on = on();
    let feed = FakeFacebook::new();
    let (ex, _, images) = exec(&feed, "badmedia");
    let elsewhere = std::env::temp_dir().join(format!("aqua_fb_elsewhere_{}.png", std::process::id()));
    image::RgbImage::new(8, 8).save(&elsewhere).unwrap();
    let txt = images.join("notes.txt");
    std::fs::write(&txt, "hello").unwrap();
    let big = images.join("big.png");
    std::fs::write(&big, vec![0u8; (MAX_MEDIA_BYTES + 1) as usize]).unwrap();
    for (media, needle) in [(elsewhere.to_string_lossy().to_string(), "not a file from elsewhere"), (txt.to_string_lossy().to_string(), "PNG or JPG"), (big.to_string_lossy().to_string(), "MB"), (images.join("missing.png").to_string_lossy().to_string(), "does not exist")] {
        let e = run(&ex, fb("publish", json!({ "text": "hi there friends", "media": media }))).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.contains(needle)), "{media}: {e:?}");
        assert_eq!(classify(e.message()), ErrorClass::InvalidInput);
    }
    assert_eq!(acted(&feed), 0, "nothing was opened, clicked or typed: {:?}", feed.calls.lock().unwrap());
    let _ = std::fs::remove_file(&elsewhere);
    let _ = std::fs::remove_file(&big);
}

#[test]
fn a_post_that_is_empty_or_too_long_or_asks_for_something_unbuilt_is_refused_up_front() {
    let _on = on();
    let feed = FakeFacebook::new();
    let (ex, _, _) = exec(&feed, "refuse");
    for (cfg, needle) in [
        (fb("publish", json!({ "text": "   " })), "no text"),
        (fb("publish", json!({ "text": "x".repeat(63_207) })), "at most 63206"),
        (fb("create_post", json!({ "text": "x".repeat(63_207) })), "at most 63206"),
        (fb("delete_post", json!({})), "cannot \"delete_post\""),
        (fb("read_comments", json!({ "post_url": "https://www.facebook.com/p/1" })), "cannot \"read_comments\""),
        (fb("reply_comment", json!({ "post_url": "https://www.facebook.com/p/1", "comment_id": "c1", "reply_text": "thanks" })), "cannot \"reply_comment\""),
        (fb("get_post", json!({ "post_url": "https://evil.example/p" })), "address of a Facebook post"),
        (fb("get_post", json!({ "post_url": "https://www.facebook.com.evil.example/p" })), "address of a Facebook post"),
        (fb("get_post", json!({ "post_url": "http://127.0.0.1.evil.example/p" })), "address of a Facebook post"),
        (fb("upload_media", json!({})), "no image to attach"),
        (fb("publish", json!({ "text": "hello friends", "mode": "connector" })), "no connector"),
    ] {
        let e = run(&ex, cfg.clone()).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.contains(needle)), "{cfg}: {e:?}");
    }
    assert_eq!(acted(&feed), 0, "{:?}", feed.calls.lock().unwrap());
}

#[test]
fn reading_a_post_opens_the_address_and_presses_nothing() {
    let _on = on();
    let feed = FakeFacebook::new();
    let (ex, _, _) = exec(&feed, "getpost");
    let r = run(&ex, fb("get_post", json!({ "post_url": "http://127.0.0.1:8792/facebook-standin.html?post=1" }))).unwrap();
    assert!(r.outputs["status"].as_str().unwrap().starts_with("read:"));
    assert_eq!(r.outputs["post_url"], "http://127.0.0.1:8792/facebook-standin.html?post=1");
    assert_eq!((feed.count("browser_navigate"), feed.count("browser_click"), feed.count("browser_type")), (1, 0, 0));
}

#[test]
fn drafting_and_attaching_can_be_separate_steps_and_publish_then_only_presses_post() {
    let _on = on();
    let feed = FakeFacebook::new();
    let (ex, _, images) = exec(&feed, "steps");
    let img = png(&images, "c.png");
    let a = run(&ex, fb("create_post", json!({ "text": "Draft words for the post", "hashtags": "news" }))).unwrap();
    assert!(a.outputs["status"].as_str().unwrap().starts_with("drafted"));
    assert_eq!(*feed.state.lock().unwrap(), State::Composer, "the post window is left open, nothing published");
    assert_eq!(feed.clicked("Post"), 0);
    run(&ex, fb("upload_media", json!({ "media": img.to_string_lossy() }))).unwrap();
    assert!(feed.attached.lock().unwrap().is_some());
    assert_eq!(feed.count("browser_navigate"), 1, "the second step used the window that was open");
    let typed_before = feed.count("browser_type");
    let p = run(&ex, fb("publish", json!({}))).unwrap();
    assert!(p.outputs["status"].as_str().unwrap().starts_with("published"));
    assert_eq!(feed.count("browser_type"), typed_before, "publish did not type again");
    assert_eq!(feed.count("browser_navigate"), 1);
    assert_eq!(feed.clicked("Post"), 1);
}

#[test]
fn a_post_window_left_over_from_another_run_is_never_the_one_that_gets_posted() {
    let _on = on();
    let feed = FakeFacebook::new();
    let (ex, _, _) = exec(&feed, "stale");
    // An earlier run left its window open with its own words in it.
    *feed.state.lock().unwrap() = State::Composer;
    *feed.typed.lock().unwrap() = "OLD DRAFT from an earlier run".into();
    let r = run(&ex, fb("publish", json!({ "text": "My real post, in my own words" }))).unwrap();
    assert!(r.outputs["status"].as_str().unwrap().starts_with("published"));
    assert_eq!(*feed.typed.lock().unwrap(), "My real post, in my own words", "the old window was thrown away and this run's words were written");
    // Publishing with no text of its own, when this run made no draft, has nothing to post.
    let feed = FakeFacebook::new();
    let (ex, _, _) = exec(&feed, "stale2");
    *feed.state.lock().unwrap() = State::Composer;
    *feed.typed.lock().unwrap() = "OLD DRAFT".into();
    let e = run(&ex, fb("publish", json!({}))).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("no text")), "{e:?}");
    assert_eq!(feed.count("browser_click"), 0, "and nothing was pressed");
}

// ---- when things go wrong -------------------------------------------------------------------------------------------------------------

#[test]
fn a_sign_in_form_a_security_check_or_an_identity_check_stops_it_and_never_types_a_password() {
    let _on = on();
    for (feed, needle) in [
        (FakeFacebook::with(|f| f.signed_out = true), "sign in there in the Aqua browser"),
        (FakeFacebook::with(|f| f.security_check = true), "verification"),
        (FakeFacebook::with(|f| f.wall = Some("Confirm your identity")), "Complete it yourself in the Aqua browser"),
        (FakeFacebook::with(|f| f.wall = Some("Your account has been locked")), "Complete it yourself in the Aqua browser"),
    ] {
        let (ex, _, _) = exec(&feed, "auth");
        let e = run(&ex, fb("publish", json!({ "text": "hello friends" }))).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.contains("not logged in") && m.contains(needle)), "{needle}: {e:?}");
        assert_eq!(classify(e.message()), ErrorClass::Authentication, "so it is a login problem for the person, not something to repair or retry");
        assert_eq!(feed.count("browser_type"), 0, "nothing was typed");
        assert_eq!(feed.count("browser_click"), 0, "nothing was clicked");
    }
}

#[test]
fn pressing_post_without_the_page_showing_it_is_unknown_never_success_and_never_repeated_by_itself() {
    let _on = on();
    // No notice and nothing new in the feed.
    let feed = FakeFacebook::with(|f| {
        f.show_notice = false;
        f.show_in_feed = false;
    });
    let (ex, _, _) = exec(&feed, "silent");
    let e = run(&ex, fb("publish", json!({ "text": "A post that never shows up" }))).unwrap_err();
    assert!(matches!(&e, NodeError::Unknown(m) if m.contains("not known whether it was published") && m.contains("Check your Facebook profile")), "{e:?}");
    assert_eq!(feed.clicked("Post"), 1);
    assert_eq!(feed.count("browser_wait_for"), 3, "every known wording was waited for");
    // A lost answer to the click itself is also unknown, not a retry.
    let feed = FakeFacebook::new();
    *feed.post_error.lock().unwrap() = Some(BrowserErrorCode::Timeout);
    let (ex, _, _) = exec(&feed, "lost");
    let e = run(&ex, fb("publish", json!({ "text": "A post whose answer is lost" }))).unwrap_err();
    assert!(matches!(e, NodeError::Unknown(_)), "{e:?}");
    assert_eq!(feed.count("browser_click"), 2, "the box and the one Post press, no more");
}

#[test]
fn a_notice_with_the_post_window_still_open_is_not_a_published_post() {
    let _on = on();
    let feed = FakeFacebook::with(|f| f.stay_open = true);
    let (ex, _, _) = exec(&feed, "open");
    let e = run(&ex, fb("publish", json!({ "text": "A post that stays in the window" }))).unwrap_err();
    assert!(matches!(&e, NodeError::Unknown(m) if m.contains("still open") && m.contains("not known whether it was published")), "{e:?}");
    assert_eq!(feed.clicked("Post"), 1);
}

#[test]
fn the_post_showing_in_the_feed_counts_only_when_it_is_new_and_an_old_post_with_the_same_words_does_not() {
    let _on = on();
    let words = "Five ways agents save time today";
    // No notice, but the new post is at the top of the feed beside the old one with the same words: published, and said so plainly.
    let feed = FakeFacebook::with(|f| {
        f.show_notice = false;
        f.old_post = Some("Five ways agents save time today");
    });
    let (ex, _, _) = exec(&feed, "feed_new");
    let r = run(&ex, fb("publish", json!({ "text": words }))).unwrap();
    assert_eq!(r.outputs["status"], "published: the post shows in the Facebook feed");
    assert_eq!(r.outputs["post_url"], "", "no link was shown, so none is claimed");
    // No notice, and the feed holds only the old post: it is not taken for the new one.
    let feed = FakeFacebook::with(|f| {
        f.show_notice = false;
        f.show_in_feed = false;
        f.old_post = Some("Five ways agents save time today");
    });
    let (ex, _, _) = exec(&feed, "feed_old");
    let e = run(&ex, fb("publish", json!({ "text": words }))).unwrap_err();
    assert!(matches!(&e, NodeError::Unknown(m) if m.contains("did not show that the post is shared")), "{e:?}");
    // A very short post is never looked for in the feed: with no notice it stays unknown.
    let feed = FakeFacebook::with(|f| f.show_notice = false);
    let (ex, _, _) = exec(&feed, "feed_short");
    let e = run(&ex, fb("publish", json!({ "text": "Hi" }))).unwrap_err();
    assert!(matches!(e, NodeError::Unknown(_)), "{e:?}");
}

#[test]
fn when_the_layout_changed_it_says_what_is_there_instead_and_does_nothing_else() {
    let _on = on();
    // The "What's on your mind" box has another name.
    let feed = FakeFacebook::new();
    *feed.start_name.lock().unwrap() = Some("Create".into());
    let (ex, _, _) = exec(&feed, "layout");
    let e = run(&ex, fb("publish", json!({ "text": "hello friends" }))).unwrap_err();
    let NodeError::Fatal(m) = &e else { panic!("{e:?}") };
    assert!(m.contains("element not found on the page") && m.contains("What's on your mind") && m.contains("\"Create\""), "{m}");
    assert_eq!(classify(m), ErrorClass::WebsiteUiChange);
    assert_eq!((feed.count("browser_type"), feed.count("browser_click")), (0, 0));
    // The Post button has another name: the text was typed, and nothing was pressed.
    let feed = FakeFacebook::new();
    *feed.post_name.lock().unwrap() = "Share now".into();
    let (ex, _, _) = exec(&feed, "layout2");
    let e = run(&ex, fb("publish", json!({ "text": "hello friends" }))).unwrap_err();
    let NodeError::Fatal(m) = &e else { panic!("{e:?}") };
    assert!(m.contains("element not found on the page") && m.contains("the \"Post\" button"), "{m}");
    assert_eq!(classify(m), ErrorClass::WebsiteUiChange);
    assert_eq!(feed.clicked("Share now"), 0);
    assert_eq!(feed.clicked("Post"), 0);
    assert_eq!(*feed.state.lock().unwrap(), State::Composer, "it was not posted");
}

#[test]
fn a_button_that_only_contains_the_word_post_is_never_pressed() {
    let _on = on();
    let feed = FakeFacebook::new();
    *feed.post_name.lock().unwrap() = "Posting\u{2026}".into();
    let (ex, _, _) = exec(&feed, "posting");
    let e = run(&ex, fb("publish", json!({ "text": "hello friends" }))).unwrap_err();
    let NodeError::Fatal(m) = &e else { panic!("{e:?}") };
    assert!(m.contains("the \"Post\" button") && m.contains("Posting") && m.contains("Nothing was pressed"), "{m}");
    assert_eq!(classify(m), ErrorClass::WebsiteUiChange);
    assert_eq!(feed.clicked("Posting\u{2026}"), 0, "{:?}", feed.approved_clicks.lock().unwrap());
    assert_eq!(*feed.state.lock().unwrap(), State::Composer);
}

#[test]
fn a_refused_approval_or_blocked_site_is_final_and_reads_as_a_permission_problem() {
    let _on = on();
    let feed = FakeFacebook::new();
    *feed.post_error.lock().unwrap() = Some(BrowserErrorCode::ConfirmationDenied);
    let (ex, _, _) = exec(&feed, "denied");
    let e = run(&ex, fb("publish", json!({ "text": "hello friends" }))).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(_)), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::PermissionError);
    assert_eq!(*feed.state.lock().unwrap(), State::Composer, "it was not posted");
    assert!(feed.in_feed.lock().unwrap().is_none());
}

// ---- the switch -----------------------------------------------------------------------------------------------------------------------

#[test]
fn the_switch_is_on_only_for_a_clear_yes() {
    for v in [Some("on"), Some("ON"), Some(" on "), Some("1"), Some("true"), Some("Yes")] {
        assert!(platform::switch_is_on(v), "{v:?}");
    }
    for v in [None, Some(""), Some("off"), Some("0"), Some("false"), Some("no"), Some("enabled"), Some("onn")] {
        assert!(!platform::switch_is_on(v), "{v:?}");
    }
}

#[test]
fn with_the_switch_off_facebook_is_refused_exactly_as_before_and_nothing_is_touched() {
    let _off = test_switch::set_facebook(false);
    let facebook = platform::get("facebook").unwrap();
    assert_eq!(platform::publishable_ids(), vec!["linkedin"]);
    assert!(!platform::can_publish(facebook));
    assert_eq!(platform::not_built_note(facebook).as_deref(), Some("posting to Facebook is not built yet (only LinkedIn can be posted to today)"));
    // The actions the table lists are what they were: the switch changes nothing else.
    assert_eq!(platform::available_actions(facebook), platform::BROWSER_ACTIONS.to_vec());
    let feed = FakeFacebook::new();
    let (ex, _, _) = exec(&feed, "off");
    for action in ["publish", "create_post", "get_post"] {
        let e = run(&ex, fb(action, json!({ "text": "Hello from Aqua", "post_url": "https://www.facebook.com/p/1" }))).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m == "invalid setting: posting to Facebook is not built yet (only LinkedIn can be posted to today). Nothing was done."), "{action}: {e:?}");
    }
    assert!(feed.calls.lock().unwrap().is_empty(), "the page was never opened");
}

#[test]
fn with_the_switch_on_facebook_is_publishable_and_every_other_platform_is_still_refused() {
    let _on = on();
    let facebook = platform::get("facebook").unwrap();
    assert_eq!(platform::publishable_ids(), vec!["linkedin", "facebook"]);
    assert!(platform::can_publish(facebook) && platform::not_built_note(facebook).is_none());
    let instagram = platform::get("instagram").unwrap();
    assert!(!platform::can_publish(instagram));
    assert_eq!(platform::not_built_note(instagram).as_deref(), Some("posting to Instagram is not built yet (only LinkedIn and Facebook can be posted to today)"));
    // What Facebook can be asked to do is what the recipe does: it does not read or answer comments.
    let actions = platform::available_actions(facebook);
    assert!(["create_post", "upload_media", "publish", "get_post"].iter().all(|a| actions.contains(a)), "{actions:?}");
    assert!(!actions.contains(&"read_comments") && !actions.contains(&"reply_comment"), "{actions:?}");
    assert!(platform::available_actions(platform::get("linkedin").unwrap()).contains(&"read_comments"), "LinkedIn is unchanged");
    // The step: Facebook runs, Instagram says it is not built.
    let feed = FakeFacebook::new();
    let (ex, _, _) = exec(&feed, "on");
    let e = run(&ex, json!({ "platform": "instagram", "action": "publish", "text": "hello friends", "media": "x.png" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("posting to Instagram is not built yet (only LinkedIn and Facebook can be posted to today)")), "{e:?}");
    assert_eq!(acted(&feed), 0);
    let r = run(&ex, fb("publish", json!({ "text": "Hello from Aqua, with the switch on" }))).unwrap();
    assert!(r.outputs["status"].as_str().unwrap().starts_with("published"));
}

#[test]
fn an_executor_made_with_the_switch_off_refuses_facebook_whatever_the_switch_says_later() {
    let feed = FakeFacebook::new();
    let ex = {
        let _off = test_switch::set_facebook(false);
        exec(&feed, "made_off").0
    };
    let _on = on();
    let e = run(&ex, fb("publish", json!({ "text": "hello friends" }))).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("Nothing was done")), "{e:?}");
    assert_eq!(acted(&feed), 0);
}

fn now() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap().and_hms_opt(15, 0, 0).unwrap()
}

fn create(text: &str) -> Proposal {
    plan(text, None, &Answers::new(), now(), None, &[])
}

#[test]
fn the_planner_and_the_checker_say_facebook_is_not_built_with_the_switch_off_and_do_not_with_it_on() {
    let request = "Write a Facebook post about AI and publish it";
    {
        let _off = test_switch::set_facebook(false);
        let p = create(request);
        assert!(p.warnings.iter().any(|w| w.contains("posting to Facebook is not built yet (only LinkedIn can be posted to today)")), "{:?}", p.warnings);
        assert!(p.validation.issues.iter().any(|i| i.code == "platform_not_built" && i.message.contains("refused before it starts")), "{:#?}", p.validation.issues);
    }
    {
        let _on = on();
        let p = create(request);
        assert!(p.understood, "{:?}", p.explanation);
        let said: Vec<&String> = p.warnings.iter().filter(|w| w.contains("Facebook")).collect();
        assert!(said.iter().any(|w| w.contains("uses the account saved as \"facebook/main\"") && w.contains("it will use the browser")), "{:?}", p.warnings);
        assert!(!said.iter().any(|w| w.contains("not built yet")), "{:?}", p.warnings);
        assert!(!p.validation.issues.iter().any(|i| i.code == "platform_not_built"), "{:#?}", p.validation.issues);
        assert!(p.validation.ok(), "{:#?}", p.validation.issues);
    }
}

#[test]
fn the_checker_knows_what_a_facebook_step_can_do_only_with_the_switch_on() {
    let flow = |action: &str| -> FlowDef {
        serde_json::from_value(json!({
            "name": "Facebook", "settings": { "approval": "always" },
            "nodes": [
                { "id": "start", "kind": "trigger", "config": { "trigger_type": "manual" } },
                { "id": "post", "kind": "social_media", "name": "Post on Facebook", "config": { "platform": "facebook", "action": action, "text": "Hello", "post_url": "https://www.facebook.com/p/1", "comment_id": "c1" } }
            ],
            "edges": [{ "from": "start", "to": "post" }]
        }))
        .unwrap()
    };
    let codes = |action: &str| -> Vec<String> { validate(&flow(action)).issues.into_iter().map(|i| i.code).collect() };
    {
        let _off = test_switch::set_facebook(false);
        assert!(codes("publish").contains(&"platform_not_built".to_string()));
        let read = codes("read_comments");
        assert!(read.contains(&"platform_not_built".to_string()) && !read.contains(&"unsupported_action".to_string()), "off: as before: {read:?}");
    }
    {
        let _on = on();
        let publish = codes("publish");
        assert!(!publish.contains(&"platform_not_built".to_string()) && !publish.contains(&"unsupported_action".to_string()), "{publish:?}");
        let report = validate(&flow("read_comments"));
        assert!(report.issues.iter().any(|i| i.code == "unsupported_action" && i.message.contains("Facebook cannot \"read_comments\" yet")), "{:#?}", report.issues);
    }
}

// ---- inside a real run --------------------------------------------------------------------------------------------------------------

fn post_flow(approval: &str) -> FlowDef {
    serde_json::from_value(json!({
        "name": "Facebook post", "settings": { "approval": approval },
        "nodes": [
            { "id": "start", "kind": "trigger", "config": { "trigger_type": "manual" } },
            { "id": "post", "kind": "social_media", "name": "Post on Facebook", "config": { "platform": "facebook", "action": "publish", "text": "Hello from a flow", "hashtags": "flows" } },
            { "id": "say", "kind": "notification", "config": { "channel": "in_app", "message": "posted: {{nodes.post.post_url}}" } }
        ],
        "edges": [{ "from": "start", "to": "post" }, { "from": "post", "to": "say" }]
    }))
    .unwrap()
}

/// A runner over the fake page for a flow. Make it after the switch is set: the step's executor reads the switch when it is made.
fn rig(name: &str, feed: &Arc<FakeFacebook>, def: FlowDef) -> (Arc<super::run::manager::FlowRunner>, Arc<super::run::store::RunStore>, i64) {
    use super::run::manager::FlowRunner;
    use super::run::store::RunStore;
    use super::store::FlowStore;
    let dir = std::env::temp_dir().join(format!("aqua_facebook_run_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("flows.db");
    let (flows, runs) = (Arc::new(FlowStore::new(&path)), Arc::new(RunStore::new(&path)));
    let (ex, _, _) = exec(feed, &format!("files_{name}"));
    let runner = FlowRunner::with_clock(flows.clone(), runs.clone(), Arc::new(Executors::new().with(ex)), Arc::new(|_, c| !c.load(std::sync::atomic::Ordering::SeqCst)), Arc::new(|| 1_800_000_000));
    let id = flows.create("local", def, "user", "", 1_800_000_000).unwrap().id;
    (runner, runs, id)
}

#[test]
fn a_facebook_post_asks_for_approval_first_and_the_approved_press_is_the_only_click_let_through() {
    use super::run::model::*;
    let _on = on();
    let feed = FakeFacebook::new();
    let (runner, runs, id) = rig("gate", &feed, post_flow("always"));
    let run = runner.start("local", id, Default::default(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(std::time::Duration::from_secs(15)));
    assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Waiting, "it stops before touching Facebook");
    assert_eq!(acted(&feed), 0);
    let gate = runs.node_run(run.id, "post").unwrap().unwrap();
    assert!(matches!(&gate.wait, Some(WaitRequest::Approval { implicit: true, preview, .. }) if preview.contains("facebook") && preview.contains("Hello from a flow")), "{:?}", gate.wait);
    runner.respond("local", run.id, "post", Response { decision: Some("approved".into()), ..Default::default() }).unwrap();
    assert!(runner.wait_idle(std::time::Duration::from_secs(15)));
    let done = runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(done.status, RunStatus::Succeeded, "{}", done.error);
    let clicks = feed.approved_clicks.lock().unwrap().clone();
    assert!(!clicks.is_empty() && clicks.iter().all(|(_, a)| *a), "clicks inside an approved post step are covered: {clicks:?}");
    let events: Vec<String> = runs.events("local", run.id, 0, 100).unwrap().into_iter().map(|e| e.message).collect();
    assert!(events.iter().any(|m| m.contains("posted: https://www.facebook.com/posts/12345")), "{events:?}");
    let nodes = runs.node_runs(run.id).unwrap();
    let v = super::supervisor::judge(&done, &nodes, &done.def);
    assert_eq!(v.kind, super::supervisor::VerdictKind::Success, "{v:?}");
    assert!(v.evidence.iter().any(|e| e.contains("post_url")));
}

#[test]
fn a_facebook_press_with_no_confirmation_stops_for_a_person_and_a_recorded_post_is_never_sent_twice() {
    use super::run::model::*;
    let _on = on();
    let feed = FakeFacebook::with(|f| {
        f.show_notice = false;
        f.show_in_feed = false;
    });
    let (runner, runs, id) = rig("unsure", &feed, post_flow("never"));
    let run = runner.start("local", id, Default::default(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(std::time::Duration::from_secs(15)));
    let n = runs.node_run(run.id, "post").unwrap().unwrap();
    assert_eq!(n.status, NodeStatus::NeedsAction, "not success, not a silent failure");
    assert!(matches!(&n.wait, Some(WaitRequest::Failure { message, choices }) if message.contains("Check whether it happened") && choices.contains(&"done".to_string())), "{:?}", n.wait);
    assert_eq!(feed.clicked("Post"), 1);
    // The person checked: it did go out. "It was done" moves on, and Post is not pressed again.
    runner.respond("local", run.id, "post", Response { choice: Some("done".into()), ..Default::default() }).unwrap();
    assert!(runner.wait_idle(std::time::Duration::from_secs(15)));
    assert_eq!(feed.clicked("Post"), 1, "never pressed twice");
    assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Succeeded);
}

#[test]
fn a_dry_run_of_a_facebook_flow_shows_the_post_and_opens_nothing() {
    use super::run::model::*;
    let _on = on();
    let feed = FakeFacebook::new();
    let (runner, runs, id) = rig("dry", &feed, post_flow("always"));
    let run = runner.start("local", id, Default::default(), Value::Null, true, "manual").unwrap();
    assert!(runner.wait_idle(std::time::Duration::from_secs(15)));
    assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Succeeded);
    assert!(feed.calls.lock().unwrap().is_empty(), "a dry run does not even open Facebook");
    let events: Vec<String> = runs.events("local", run.id, 0, 100).unwrap().into_iter().map(|e| e.message).collect();
    assert!(events.iter().any(|m| m.contains("Would run \"Post on Facebook\" here. Nothing was sent.")), "{events:?}");
    let nodes = runs.node_runs(run.id).unwrap();
    let done = runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(super::supervisor::judge(&done, &nodes, &done.def).kind, super::supervisor::VerdictKind::Unknown, "a dry run proves nothing");
}

#[test]
fn a_flow_that_posts_to_facebook_is_refused_before_it_starts_with_the_switch_off_and_starts_with_it_on() {
    use super::run::model::*;
    let feed = FakeFacebook::new();
    {
        let _off = test_switch::set_facebook(false);
        let (runner, runs, id) = rig("refused", &feed, post_flow("always"));
        let err = runner.start("local", id, Default::default(), Value::Null, false, "manual").unwrap_err();
        assert!(err.contains("\"Post on Facebook\"") && err.contains("posting to Facebook is not built yet (only LinkedIn can be posted to today)") && err.contains("Nothing was started"), "{err}");
        assert!(runs.list_runs("local", Some(id), 10).unwrap().is_empty(), "no run was made, so nobody was asked anything");
        assert!(feed.calls.lock().unwrap().is_empty(), "no page was opened");
        // A dry run still works.
        let run = runner.start("local", id, Default::default(), Value::Null, true, "manual").unwrap();
        assert!(runner.wait_idle(std::time::Duration::from_secs(15)));
        assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Succeeded);
        assert!(feed.calls.lock().unwrap().is_empty());
    }
    {
        let _on = on();
        let (runner, runs, id) = rig("started", &feed, post_flow("always"));
        let run = runner.start("local", id, Default::default(), Value::Null, false, "manual").expect("with the switch on the run starts");
        assert!(runner.wait_idle(std::time::Duration::from_secs(15)));
        assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Waiting, "and it asks the person before it touches Facebook");
        assert_eq!(acted(&feed), 0);
    }
}
