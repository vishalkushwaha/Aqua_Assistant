//! Exploring a page, read-only (card R7-10).
//!
//! The explorer is given a `ReadOnlyPage`. The trait has `open` (once, for the address the person gave), `snapshot` and `find`, and no way to click, type, press, choose or upload,
//! so the explorer cannot act on a page whatever the page says. What it hands back is data: the controls that look like a way to write and send a text, ranked by a simple score
//! from fixed word lists, and any wall the page shows. No control is aimed at by position, a reference or a selector: only by role and visible name.
//!
//! Everything is capped (pages, characters, controls, names) and everything taken from a page is scrubbed of secrets and cut. Text on the page is never used as an instruction.

use super::{clean_line, clean_name, norm, words, MAX_CANDIDATES, MAX_ELEMENTS_PER_PAGE, MAX_GOAL_CHARS, MAX_NAME_CHARS, MAX_PAGE_CHARS, MAX_SNAPSHOTS, MAX_URL_CHARS};
use crate::flows::browser_exec::{elements, looks_signed_out};
use aqua_mcp_adapters::browser::{BrowserErrorCode, BrowserObservation};
use serde::Serialize;

/// What a page shows: its address and its accessibility snapshot as text (lines such as `- button "Publish" [ref=e5]`).
#[derive(Debug, Clone, PartialEq)]
pub struct PageView {
    pub url: String,
    pub text: String,
}

/// A page the explorer may look at and may not touch. There is deliberately no method that acts on the page: this is what makes the exploration read-only.
pub trait ReadOnlyPage {
    /// Go to the address the person gave. The only navigation the explorer asks for, and it asks once.
    fn open(&self, url: &str) -> Result<(), String>;
    /// The page as it is now.
    fn snapshot(&self) -> Result<PageView, String>;
    /// The part of the page that matches a word (and a role, when one is given): a small look at a long page.
    fn find(&self, role: Option<&str>, name: &str) -> Result<PageView, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    /// A place to write the text.
    TextBox,
    /// A button or link that opens the place to write it (a window that is not open yet).
    Opener,
    /// The button that sends or publishes it.
    SendButton,
}

/// One control that may belong to the recipe. `why` is built from the fixed word lists only, never from the page.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub role: String,
    /// The visible name, scrubbed and cut to `MAX_NAME_CHARS`. This is the only thing taken from the page's own words.
    pub name: String,
    pub score: u32,
    /// How many controls with this role and name the page showed (a recipe refuses to guess between two).
    pub count: usize,
    pub why: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WallKind {
    /// A sign-in form.
    SignIn,
    /// A captcha or a security check.
    Verification,
    /// A lock, a block, an identity check.
    Blocked,
}

/// A page that asks the person for something. Nothing is drafted past it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Wall {
    pub kind: WallKind,
    /// The sign that gave it away, from the fixed word lists.
    pub sign: String,
}

impl Wall {
    pub fn describe(&self) -> String {
        match self.kind {
            WallKind::SignIn => "a sign-in form".to_string(),
            WallKind::Verification => format!("a verification or security check (\"{}\")", self.sign),
            WallKind::Blocked => format!("a lock or a block (\"{}\")", self.sign),
        }
    }
}

/// What exploring found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Exploration {
    pub goal: String,
    pub start_url: String,
    /// The host of the start address.
    pub site: String,
    /// Pages read, the first one included.
    pub snapshots: usize,
    /// Best first. At most four of each kind.
    pub candidates: Vec<Candidate>,
    pub wall: Option<Wall>,
    /// A cookie or consent notice was showing (the recipe does not click it).
    pub consent_notice: bool,
    /// Text boxes and buttons with no name: a recipe cannot aim at them.
    pub unnamed_controls: usize,
    /// A cap cut something off (too many controls, or the look stopped at `MAX_SNAPSHOTS`).
    pub capped: bool,
    /// The host the page was really at, when it is not the host that was asked for.
    pub redirected_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExploreError {
    BadAddress(String),
    NoGoal,
    Page(String),
}

impl std::fmt::Display for ExploreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExploreError::BadAddress(m) | ExploreError::Page(m) => f.write_str(m),
            ExploreError::NoGoal => f.write_str("there is no goal to explore for"),
        }
    }
}

impl std::error::Error for ExploreError {}

// ---- the word lists -----------------------------------------------------------------------------------------------------------

/// Names a send button has (the whole name).
pub(crate) const SEND_WORDS: &[&str] = &["post", "publish", "share", "send", "tweet", "submit"];
/// Words that may follow one of those in a send button's name ("Post it", "Publish now").
pub(crate) const SEND_TAILS: &[&str] = &["it", "now", "post", "note", "message", "update", "tweet", "story", "text"];
/// What a button or link that opens the writing window is called.
const OPENER_PHRASES: &[&str] = &[
    "what's on your mind",
    "what's happening",
    "start a post",
    "create a post",
    "create post",
    "new post",
    "write a post",
    "write something",
    "compose",
    "new note",
    "create a note",
    "start writing",
    "say something",
];
/// Words in a text box's name that make it more likely to be the place to write.
const COMPOSE_PHRASES: &[&str] = &["what's on your mind", "what's happening", "write", "compose", "say something", "share", "post", "message", "note", "thought", "status", "update"];
/// A text box with one of these words in its name is for something else.
const EXCLUDED_BOX_WORDS: &[&str] = &[
    "search", "find", "filter", "password", "passcode", "email", "username", "phone", "mobile", "captcha", "code", "otp", "coupon", "zip", "postcode", "name", "login", "comment", "reply",
];
const CONSENT_WORDS: &[&str] = &["accept all cookies", "accept cookies", "we use cookies", "cookie settings", "manage cookies", "cookie preferences", "cookie consent", "manage consent", "your privacy choices"];
const VERIFY_WORDS: &[&str] = &["captcha", "security check", "verify it's you", "are you a robot", "not a robot", "unusual traffic", "checkpoint"];
const BLOCK_WORDS: &[&str] = &[
    "confirm your identity",
    "confirm it's you",
    "enter the code",
    "security code",
    "approve your login",
    "your account has been locked",
    "your account has been disabled",
    "your account has been suspended",
    "temporarily blocked",
    "access denied",
    "you have been blocked",
];
/// What to look for when the page did not show a kind of control. A look costs one page of the budget.
const QUERIES_TEXTBOX: &[&str] = &["write", "compose", "message"];
const QUERIES_SEND: &[&str] = &["post", "publish", "share", "send"];
const QUERIES_OPENER: &[&str] = &["create", "start a post", "what's on your mind"];
/// Words of a goal that say it writes a text and sends it. The only kind of goal this builder drafts.
pub(crate) const PUBLISH_GOAL_WORDS: &[&str] = &["post", "publish", "share", "send", "tweet", "write", "compose", "submit", "update"];
/// Words of a goal that tell nothing about which control is meant.
const STOP_WORDS: &[&str] = &["that", "this", "with", "from", "into", "your", "some", "something", "text", "about", "please", "want", "make", "then"];

/// At most this many candidates of one kind are kept.
const PER_KIND: usize = 4;

// ---- the address and the page -------------------------------------------------------------------------------------------------

/// The start address if it is a plain web address with a host and no login in it. Returns the address and its host.
pub(crate) fn check_address(raw: &str) -> Result<(String, String), ExploreError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().count() > MAX_URL_CHARS || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ExploreError::BadAddress("the address is empty, too long or has a space in it".into()));
    }
    let u = url::Url::parse(raw).map_err(|_| ExploreError::BadAddress("that is not a web address".into()))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(ExploreError::BadAddress("only http and https addresses are explored".into()));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(ExploreError::BadAddress("an address with a user name or a password in it is refused".into()));
    }
    let Some(host) = u.host_str() else { return Err(ExploreError::BadAddress("the address has no host".into())) };
    Ok((raw.to_string(), host.trim_end_matches('.').to_ascii_lowercase()))
}

/// A view cut to what is read of a page, with its address scrubbed.
pub(crate) fn capped_view(v: PageView) -> PageView {
    let url = clean_line(&v.url, MAX_URL_CHARS);
    if v.text.chars().count() <= MAX_PAGE_CHARS {
        PageView { url, text: v.text }
    } else {
        PageView { url, text: v.text.chars().take(MAX_PAGE_CHARS).collect() }
    }
}

/// Does the page ask for something only the person can give? The sign-in test is the recipes' own (`looks_signed_out`); the other words are the ones the recipes stop at.
pub(crate) fn detect_wall(url: &str, text: &str) -> Option<Wall> {
    let mut o = BrowserObservation::failure("browser_snapshot", "recipe_builder", BrowserErrorCode::BackendError, "");
    o.ok = true;
    o.error = None;
    o.url = Some(url.to_string());
    if looks_signed_out(&o, text) {
        return Some(Wall { kind: WallKind::SignIn, sign: "a sign-in form".into() });
    }
    let lower = norm(text);
    if let Some(w) = VERIFY_WORDS.iter().find(|w| lower.contains(**w)) {
        return Some(Wall { kind: WallKind::Verification, sign: (*w).to_string() });
    }
    if let Some(w) = BLOCK_WORDS.iter().find(|w| lower.contains(**w)) {
        return Some(Wall { kind: WallKind::Blocked, sign: (*w).to_string() });
    }
    None
}

fn has_consent_notice(text: &str) -> bool {
    let n = norm(text);
    CONSENT_WORDS.iter().any(|w| n.contains(*w))
}

/// Text boxes and buttons that show a reference but no name.
fn unnamed_controls(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let t = l.trim_start().trim_start_matches("- ");
            (t.starts_with("textbox") || t.starts_with("button")) && t.contains("[ref=") && !t.contains('"')
        })
        .count()
}

// ---- goals ------------------------------------------------------------------------------------------------------------------

/// Does the goal write a text and send it? Only such goals are drafted.
pub(crate) fn is_publish_goal(goal: &str) -> bool {
    let n = norm(goal);
    words(&n).iter().any(|w| PUBLISH_GOAL_WORDS.contains(w))
}

/// The words of a goal that may name a control ("note" in "publish a note").
fn goal_keywords(goal: &str) -> Vec<String> {
    let n = norm(goal);
    let mut out: Vec<String> = Vec::new();
    for w in words(&n) {
        if out.len() < 8 && w.chars().count() >= 4 && !STOP_WORDS.contains(&w) && !PUBLISH_GOAL_WORDS.contains(&w) && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

// ---- scoring ----------------------------------------------------------------------------------------------------------------

/// A control as the page showed it. `count` is how many controls had this role and name on one page.
struct Seen {
    role: String,
    name: String,
    key: String,
    count: usize,
}

struct Scored {
    kind: CandidateKind,
    score: u32,
    why: String,
}

/// How well a normalised button name is a send button's: 10 for a send word alone, 6 for a send word and a usual tail, nothing otherwise. "Posting..." and "Post settings" are nothing.
pub(crate) fn send_score(n: &str) -> Option<u32> {
    if SEND_WORDS.contains(&n) {
        return Some(10);
    }
    let (first, rest) = n.split_once(' ')?;
    (SEND_WORDS.contains(&first) && SEND_TAILS.contains(&rest)).then_some(6)
}

fn why_text(what: &str, hits: &[&str], goal_hits: u32) -> String {
    let mut s = what.to_string();
    if hits.is_empty() {
        s.push_str(" with no word of the compose list in its name");
    } else {
        s.push_str(&format!(" whose name has {}", hits.iter().map(|h| format!("\"{h}\"")).collect::<Vec<_>>().join(", ")));
    }
    if goal_hits > 0 {
        s.push_str(" and a word of the goal");
    }
    s
}

fn classify(role: &str, name: &str, goal_words: &[String]) -> Option<Scored> {
    let n = norm(name);
    let ws = words(&n);
    let goal_hits = ws.iter().filter(|w| goal_words.iter().any(|g| g.as_str() == **w)).count() as u32;
    match role {
        "textbox" => {
            if ws.iter().any(|w| EXCLUDED_BOX_WORDS.contains(w)) {
                return None;
            }
            let hits: Vec<&str> = COMPOSE_PHRASES.iter().copied().filter(|p| n.contains(*p)).collect();
            Some(Scored { kind: CandidateKind::TextBox, score: 1 + 3 * hits.len() as u32 + 2 * goal_hits, why: why_text("a text box", &hits, goal_hits) })
        }
        "button" | "link" => {
            if role == "button" {
                if let Some(base) = send_score(&n) {
                    let what = if base == 10 { "a button named with a send word alone" } else { "a button named with a send word and its usual tail" };
                    return Some(Scored { kind: CandidateKind::SendButton, score: base + goal_hits, why: if goal_hits > 0 { format!("{what} and a word of the goal") } else { what.to_string() } });
                }
            }
            let hits: Vec<&str> = OPENER_PHRASES.iter().copied().filter(|p| n.contains(*p)).collect();
            if hits.is_empty() {
                return None;
            }
            Some(Scored { kind: CandidateKind::Opener, score: 4 + 2 * goal_hits, why: why_text(&format!("a {role} that opens the writing window"), &hits, goal_hits) })
        }
        _ => None,
    }
}

#[derive(Default, Clone, Copy)]
struct Have {
    textbox: bool,
    opener: bool,
    send: bool,
}

fn present(pool: &[Seen], goal_words: &[String]) -> Have {
    let mut h = Have::default();
    for s in pool {
        match classify(&s.role, &s.name, goal_words).map(|c| c.kind) {
            Some(CandidateKind::TextBox) => h.textbox = true,
            Some(CandidateKind::Opener) => h.opener = true,
            Some(CandidateKind::SendButton) => h.send = true,
            None => {}
        }
    }
    h
}

/// Add the named controls of a page to the pool. The same role and name twice on one page is one entry with a count; a control seen again on another page of the same look is not counted twice.
fn take_elements(text: &str, pool: &mut Vec<Seen>, capped: &mut bool) {
    let all = elements(text);
    if all.len() > MAX_ELEMENTS_PER_PAGE {
        *capped = true;
    }
    let mut here: Vec<Seen> = Vec::new();
    for e in all.into_iter().take(MAX_ELEMENTS_PER_PAGE) {
        let role = e.role.to_lowercase();
        let key = e.name.to_lowercase();
        match here.iter().position(|s| s.role == role && s.key == key) {
            Some(i) => here[i].count += 1,
            None => here.push(Seen { role, name: e.name, key, count: 1 }),
        }
    }
    for h in here {
        match pool.iter().position(|s| s.role == h.role && s.key == h.key) {
            Some(i) => pool[i].count = pool[i].count.max(h.count),
            None => pool.push(h),
        }
    }
}

/// The ranked candidates: best score first; for equal scores the control the page shows once, then the shorter name, then the page's own order. At most `PER_KIND` of each kind.
fn candidates_of(pool: &[Seen], goal_words: &[String]) -> Vec<Candidate> {
    let mut scored: Vec<(usize, Scored, &Seen)> = Vec::new();
    for (i, s) in pool.iter().enumerate() {
        if let Some(sc) = classify(&s.role, &s.name, goal_words) {
            scored.push((i, sc, s));
        }
    }
    scored.sort_by(|a, b| b.1.score.cmp(&a.1.score).then(a.2.count.cmp(&b.2.count)).then(a.2.name.chars().count().cmp(&b.2.name.chars().count())).then(a.0.cmp(&b.0)));
    let mut per_kind = [0usize; 3];
    let mut out: Vec<Candidate> = Vec::new();
    for (_, sc, s) in scored {
        let slot = sc.kind as usize;
        if per_kind[slot] >= PER_KIND || out.len() >= MAX_CANDIDATES {
            continue;
        }
        per_kind[slot] += 1;
        out.push(Candidate { kind: sc.kind, role: s.role.clone(), name: clean_name(&s.name, MAX_NAME_CHARS), score: sc.score, count: s.count, why: sc.why });
    }
    out
}

#[derive(Clone, Copy)]
enum Want {
    TextBox,
    Send,
    Opener,
}

/// Explore the page at `start_url` for a way to do `goal`. Reads at most `MAX_SNAPSHOTS` pages: the first one, then small looks (`find`) for a kind of control the first did not show.
/// A page with a wall is reported as it is and not read further. Nothing is clicked, typed or pressed: the page cannot be made to.
pub fn explore(page: &dyn ReadOnlyPage, goal: &str, start_url: &str) -> Result<Exploration, ExploreError> {
    let goal = clean_line(goal, MAX_GOAL_CHARS);
    if goal.is_empty() {
        return Err(ExploreError::NoGoal);
    }
    let (start, site) = check_address(start_url)?;
    page.open(&start).map_err(|e| ExploreError::Page(clean_line(&e, 200)))?;
    let first = capped_view(page.snapshot().map_err(|e| ExploreError::Page(clean_line(&e, 200)))?);
    let mut ex = Exploration { goal: goal.clone(), start_url: start, site: site.clone(), snapshots: 1, candidates: Vec::new(), wall: None, consent_notice: false, unnamed_controls: 0, capped: false, redirected_to: None };
    if let Ok((_, host)) = check_address(&first.url) {
        if host != site {
            ex.redirected_to = Some(clean_line(&host, 100));
        }
    }
    ex.wall = detect_wall(&first.url, &first.text);
    if ex.wall.is_some() {
        return Ok(ex);
    }
    ex.consent_notice = has_consent_notice(&first.text);
    ex.unnamed_controls = unnamed_controls(&first.text);
    let keywords = goal_keywords(&goal);
    let mut pool: Vec<Seen> = Vec::new();
    take_elements(&first.text, &mut pool, &mut ex.capped);
    let have = present(&pool, &keywords);
    let mut todo: Vec<(Want, &'static str)> = Vec::new();
    if !have.textbox {
        todo.extend(QUERIES_TEXTBOX.iter().map(|q| (Want::TextBox, *q)));
    }
    if !have.send {
        todo.extend(QUERIES_SEND.iter().map(|q| (Want::Send, *q)));
    }
    if !have.textbox && !have.opener {
        todo.extend(QUERIES_OPENER.iter().map(|q| (Want::Opener, *q)));
    }
    for (want, query) in todo {
        let now = present(&pool, &keywords);
        let satisfied = match want {
            Want::TextBox => now.textbox,
            Want::Send => now.send,
            Want::Opener => now.textbox || now.opener,
        };
        if satisfied {
            continue;
        }
        if ex.snapshots >= MAX_SNAPSHOTS {
            ex.capped = true;
            break;
        }
        ex.snapshots += 1;
        // A look that fails costs its page of the budget and adds nothing.
        if let Ok(view) = page.find(None, query) {
            take_elements(&capped_view(view).text, &mut pool, &mut ex.capped);
        }
    }
    ex.candidates = candidates_of(&pool, &keywords);
    Ok(ex)
}
