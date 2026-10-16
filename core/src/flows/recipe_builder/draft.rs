//! A recipe draft (card R7-10): what the explorer found, as data a person can read and a rehearsal can run.
//!
//! A draft is steps by role and name (never a position, a reference or a selector), what to check after the step that sends, the risks, and what only the person can do. It has a
//! stable text form (`render`) and a JSON form (`to_json` / `from_json`). Two rules are kept by `validate`, and every draft that is stored or rehearsed passes it:
//! - a step that sends or publishes something is marked `irreversible`, and some check follows it that tells whether it worked (a recipe that cannot tell whether it worked
//!   must not exist);
//! - the last line for the person says the recipe has never been tried on the real site.

use super::explore::{check_address, is_publish_goal, send_score, CandidateKind, Exploration};
use super::{clean_line, norm, MAX_GOAL_CHARS, MAX_LINE_CHARS, MAX_NAME_CHARS, MAX_NOTES, MAX_POSTS, MAX_STEPS};
use serde::{Deserialize, Serialize};

/// The name of the input that carries the words to post.
pub const INPUT_NAME: &str = "text";
/// The last line of every draft's `needs_person`.
pub const NEVER_TRIED: &str = "This recipe has never been tried on the real site; the first real run is yours, with your approval.";
/// Said when a step names a control that was not on the page when it was explored.
pub const EXPECTED_NOTE: &str = "Steps marked [expected] name controls that were not on the page when it was explored (a window that opens only after a click shows them). Their names are guesses from a word list. If the real names differ, the first trial stops with \"element not found\" and does nothing else.";

const DRAFT_VERSION: u32 = 1;
const MAX_JSON_BYTES: usize = 64 * 1024;
const KEYS: &[&str] = &["Enter", "Tab", "Escape"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Open,
    Click,
    Type,
    Press,
    WaitFor,
}

/// A control by accessible role and visible name. `seen` is false for a control that was not on the page when it was explored: its name is a guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub role: String,
    pub name: String,
    pub seen: bool,
}

impl Target {
    pub fn new(role: &str, name: &str, seen: bool) -> Target {
        Target { role: role.to_string(), name: name.to_string(), seen }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub action: Action,
    /// `click`, `type` and `wait_for`: the control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    /// `open`: the address (on the draft's own site).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `press`: `Enter`, `Tab` or `Escape`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// `type`: the name of the input whose words are typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<String>,
    /// The step sends or publishes something, which Aqua cannot take back.
    #[serde(default)]
    pub irreversible: bool,
    #[serde(default)]
    pub note: String,
}

impl Step {
    fn base(action: Action, note: &str) -> Step {
        Step { action, target: None, url: None, key: None, value_from: None, irreversible: false, note: note.to_string() }
    }

    pub fn open(url: &str) -> Step {
        Step { url: Some(url.to_string()), ..Step::base(Action::Open, "opens the start page") }
    }

    pub fn click(target: Target, note: &str) -> Step {
        Step { target: Some(target), ..Step::base(Action::Click, note) }
    }

    pub fn type_into(target: Target) -> Step {
        Step { target: Some(target), value_from: Some(INPUT_NAME.to_string()), ..Step::base(Action::Type, "writes the words of the post (the input called text)") }
    }

    /// The step that sends: a click, marked irreversible.
    pub fn send(target: Target) -> Step {
        Step { target: Some(target), irreversible: true, ..Step::base(Action::Click, "SENDS the post: the only step that publishes") }
    }

    /// The step in plain words, without the markers.
    pub fn describe_plain(&self) -> String {
        match self.action {
            Action::Open => format!("open {}", self.url.as_deref().unwrap_or("?")),
            Action::Click => format!("click {}", target_text(&self.target)),
            Action::Type => format!("type into {} the words of the input \"{}\"", target_text(&self.target), self.value_from.as_deref().unwrap_or("?")),
            Action::Press => format!("press {}", self.key.as_deref().unwrap_or("?")),
            Action::WaitFor => format!("wait for {}", target_text(&self.target)),
        }
    }

    /// The step with its markers: `[expected ...]` for a control that was not seen, `[IRREVERSIBLE ...]` for a step that sends.
    pub fn describe(&self) -> String {
        let mut s = self.describe_plain();
        if self.target.as_ref().is_some_and(|t| !t.seen) {
            s.push_str(" [expected: not on the page when it was explored]");
        }
        if self.irreversible {
            s.push_str(" [IRREVERSIBLE: sends something]");
        }
        s
    }
}

/// A name from the page as the person reads it, in double quotes, with any double quote inside it escaped: a name cannot close the quotes and pass for a marker of the draft.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\\\""))
}

fn target_text(t: &Option<Target>) -> String {
    match t {
        Some(t) => format!("{} {}", t.role, quote(&t.name)),
        None => "(nothing)".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostKind {
    Appears,
    Disappears,
    TextPresent,
}

/// A check that follows a step. `text_present` means the words show on the page outside a text box, more often than before the step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Post {
    pub kind: PostKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_from: Option<String>,
    /// The step this follows, counted from 0.
    pub after_step: usize,
}

impl Post {
    pub fn text_present(after_step: usize) -> Post {
        Post { kind: PostKind::TextPresent, target: None, text: None, text_from: Some(INPUT_NAME.to_string()), after_step }
    }

    pub fn disappears(after_step: usize, target: Target) -> Post {
        Post { kind: PostKind::Disappears, target: Some(target), text: None, text_from: None, after_step }
    }

    pub fn appears(after_step: usize, target: Target) -> Post {
        Post { kind: PostKind::Appears, target: Some(target), text: None, text_from: None, after_step }
    }

    pub fn describe(&self) -> String {
        match self.kind {
            PostKind::Appears => format!("the {} appears", target_text(&self.target)),
            PostKind::Disappears => format!("the {} disappears", target_text(&self.target)),
            PostKind::TextPresent => match (&self.text, &self.text_from) {
                (Some(t), _) => format!("the words {} show on the page (outside a text box)", quote(t)),
                (None, Some(k)) => format!("the words of the input \"{k}\" show on the page (outside a text box)"),
                _ => "some words show on the page".to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeDraft {
    pub version: u32,
    /// The host of the start address.
    pub site: String,
    pub goal: String,
    pub start_url: String,
    pub steps: Vec<Step>,
    pub postconditions: Vec<Post>,
    pub risks: Vec<String>,
    /// What only the person can do. The last line is always `NEVER_TRIED`.
    pub needs_person: Vec<String>,
    /// Why there are no steps, when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_reason: Option<String>,
}

fn finish(mut d: RecipeDraft) -> RecipeDraft {
    d.risks.truncate(MAX_NOTES);
    d.needs_person.truncate(MAX_NOTES - 1);
    for line in d.risks.iter_mut().chain(d.needs_person.iter_mut()) {
        let cleaned = clean_line(line, MAX_LINE_CHARS);
        *line = cleaned;
    }
    if let Some(r) = d.empty_reason.as_mut() {
        let cleaned = clean_line(r, MAX_LINE_CHARS);
        *r = cleaned;
    }
    d.needs_person.push(NEVER_TRIED.to_string());
    d
}

/// Draft a recipe for `goal` from what was explored. A goal that does not write and send a text, a page behind a wall, and a page with nothing to write in each give a draft with no
/// steps and the reason. Otherwise the draft is `open`, `type`, then the send step (marked irreversible) with the checks that follow it; when only a button that opens the writing
/// window was on the page, the text box and the send button are named from the word lists and marked as expected. Nothing here asks a model.
pub fn build_draft(goal: &str, ex: &Exploration) -> RecipeDraft {
    let goal_text = clean_line(goal, MAX_GOAL_CHARS);
    let mut d = RecipeDraft {
        version: DRAFT_VERSION,
        site: ex.site.clone(),
        goal: if goal_text.is_empty() { ex.goal.clone() } else { goal_text },
        start_url: ex.start_url.clone(),
        steps: Vec::new(),
        postconditions: Vec::new(),
        risks: Vec::new(),
        needs_person: Vec::new(),
        empty_reason: None,
    };
    if let Some(host) = &ex.redirected_to {
        d.risks.push(format!("The start address led to another site ({host}), not {}. Check that this is the site you meant.", ex.site));
    }
    if ex.unnamed_controls > 0 {
        d.risks.push(format!("{} text boxes or buttons on the page have no name. A recipe finds controls by role and name, so it cannot aim at them.", ex.unnamed_controls));
    }
    if ex.capped {
        d.risks.push("The page was larger than what is read, or the look stopped at its limit, so some controls may not have been seen.".to_string());
    }
    if ex.consent_notice {
        d.needs_person.push("A cookie or consent notice was showing. The recipe does not click it: accept or decline it yourself in the Aqua browser before the first trial.".to_string());
    }
    if !is_publish_goal(&d.goal) {
        d.empty_reason = Some(format!("This builder drafts goals that write a text and send it (post, publish, share, send). The goal \"{}\" has none of those words, so nothing was drafted.", d.goal));
        return finish(d);
    }
    if let Some(w) = &ex.wall {
        d.empty_reason = Some(format!("The page is asking for something only you can give: {}. Nothing was drafted past it.", w.describe()));
        d.needs_person.push("Sign in, or complete the check, yourself in the Aqua browser (Aqua never types a password), then explore the site again.".to_string());
        return finish(d);
    }
    let best = |k: CandidateKind| ex.candidates.iter().find(|c| c.kind == k);
    let (textbox, opener, send) = (best(CandidateKind::TextBox), best(CandidateKind::Opener), best(CandidateKind::SendButton));
    let mut expected = false;
    let send_target = match send {
        Some(s) => Target::new(&s.role, &s.name, true),
        None => {
            expected = true;
            Target::new("button", "Post", false)
        }
    };
    if let Some(tb) = textbox {
        d.steps.push(Step::open(&ex.start_url));
        d.steps.push(Step::type_into(Target::new(&tb.role, &tb.name, true)));
        d.steps.push(Step::send(send_target));
        d.postconditions.push(Post::text_present(2));
    } else if let Some(op) = opener {
        // Only the button that opens the writing window is on the page. The window's text box is named like the button (a page usually names it after its prompt), and the send button
        // "Post"; both are guesses and are marked so.
        expected = true;
        let window_box = Target::new("textbox", &op.name, false);
        d.steps.push(Step::open(&ex.start_url));
        d.steps.push(Step::click(Target::new(&op.role, &op.name, true), "opens the window to write in"));
        d.steps.push(Step::type_into(window_box.clone()));
        d.steps.push(Step::send(send_target));
        d.postconditions.push(Post::disappears(3, window_box));
        d.postconditions.push(Post::text_present(3));
    } else {
        d.empty_reason = Some(match send {
            Some(s) => format!("A button named {} was found, but no text box and no button that opens one, so there is nowhere to write the text.", quote(&s.name)),
            None => format!("No control for writing or sending a text was found on the page (after reading {} page{}).", ex.snapshots, if ex.snapshots == 1 { "" } else { "s" }),
        });
        return finish(d);
    }
    d.risks.push("The send step publishes under your name. Aqua has no way to take a post back, so a wrong press cannot be undone by Aqua.".to_string());
    d.risks.push("The site can rename or move a control at any time. A renamed control stops the recipe with \"element not found\"; it never guesses.".to_string());
    for c in ex.candidates.iter().filter(|c| c.count > 1 && d.steps.iter().any(|s| s.target.as_ref().is_some_and(|t| t.role == c.role && t.name == c.name))) {
        d.risks.push(format!("More than one {} on the page is called {}. A recipe refuses to guess between them.", c.role, quote(&c.name)));
    }
    d.needs_person.insert(0, "Read every step. Only the step marked IRREVERSIBLE sends anything, and it must ask for your approval every time it runs.".to_string());
    if expected {
        d.needs_person.push(EXPECTED_NOTE.to_string());
    }
    finish(d)
}

// ---- reading and showing a draft ----------------------------------------------------------------------------------------------

impl RecipeDraft {
    /// The draft as compact JSON. The field order is fixed, so the same draft is always the same text.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Read a draft from JSON and check it. A field this code does not know, a draft that breaks a rule of `validate`, or a draft over 64 KB is refused.
    pub fn from_json(json: &str) -> Result<RecipeDraft, String> {
        if json.len() > MAX_JSON_BYTES {
            return Err(format!("the draft is larger than {} KB", MAX_JSON_BYTES / 1024));
        }
        let d: RecipeDraft = serde_json::from_str(json).map_err(|e| format!("the draft could not be read: {e}"))?;
        d.validate()?;
        Ok(d)
    }

    /// The steps that send something, counted from 0.
    pub fn sends(&self) -> Vec<usize> {
        self.steps.iter().enumerate().filter(|(_, s)| s.irreversible).map(|(i, _)| i).collect()
    }

    /// The draft as text for the person. Stable: the same draft is always the same text.
    pub fn render(&self) -> String {
        let mut out = format!("Recipe draft for {} (goal: {})\nStatus: a draft. It has never been tried on the real site.\n", self.site, self.goal);
        if self.steps.is_empty() {
            out.push_str("\nNo steps were drafted.\n");
            if let Some(r) = &self.empty_reason {
                out.push_str(&format!("Why: {r}\n"));
            }
        } else {
            out.push_str("\nSteps\n");
            for (i, s) in self.steps.iter().enumerate() {
                out.push_str(&format!("{:>2}. {}\n", i + 1, s.describe()));
            }
        }
        if !self.postconditions.is_empty() {
            out.push_str("\nAfter a step, the recipe checks\n");
            for p in &self.postconditions {
                out.push_str(&format!("- after step {}: {}\n", p.after_step + 1, p.describe()));
            }
        }
        if !self.risks.is_empty() {
            out.push_str("\nRisks\n");
            for r in &self.risks {
                out.push_str(&format!("- {r}\n"));
            }
        }
        out.push_str("\nFor you\n");
        for n in &self.needs_person {
            out.push_str(&format!("- {n}\n"));
        }
        out
    }

    /// The rules every draft that is stored or rehearsed keeps. The first problem found, in words for the person.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != DRAFT_VERSION {
            return Err(format!("the draft is version {}, and only version {DRAFT_VERSION} is read", self.version));
        }
        text_ok("the site", &self.site, 100)?;
        text_ok("the goal", &self.goal, MAX_GOAL_CHARS)?;
        text_ok("the start address", &self.start_url, 300)?;
        let (_, host) = check_address(&self.start_url).map_err(|e| e.to_string())?;
        if host != self.site {
            return Err(format!("the site ({}) is not the host of the start address ({host})", self.site));
        }
        if self.steps.len() > MAX_STEPS || self.postconditions.len() > MAX_POSTS || self.risks.len() > MAX_NOTES || self.needs_person.len() > MAX_NOTES {
            return Err("the draft has more steps, checks or notes than a draft may have".to_string());
        }
        for line in self.risks.iter().chain(self.needs_person.iter()) {
            text_ok("a note", line, MAX_LINE_CHARS)?;
        }
        if let Some(r) = &self.empty_reason {
            text_ok("the reason", r, MAX_LINE_CHARS)?;
        }
        if self.needs_person.last().map(String::as_str) != Some(NEVER_TRIED) {
            return Err("the last line for the person must say the recipe has never been tried on the real site".to_string());
        }
        if self.steps.is_empty() {
            return if self.postconditions.is_empty() { Ok(()) } else { Err("a draft with no steps has checks".to_string()) };
        }
        for (i, s) in self.steps.iter().enumerate() {
            validate_step(s, i, self)?;
        }
        for (j, p) in self.postconditions.iter().enumerate() {
            validate_post(p, j, self.steps.len())?;
        }
        for i in self.sends() {
            if !self.postconditions.iter().any(|p| p.after_step == i) {
                return Err(format!("step {} sends something, but nothing checks that it worked. A recipe that cannot tell whether it worked must not exist", i + 1));
            }
        }
        Ok(())
    }
}

/// A text that is kept: not empty, not too long, no control character, and nothing in it the secret scrubber would change.
fn text_ok(what: &str, text: &str, max: usize) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{what} is empty"));
    }
    if text.chars().count() > max {
        return Err(format!("{what} is longer than {max} characters"));
    }
    if text.chars().any(|c| c.is_control()) {
        return Err(format!("{what} has a control character in it"));
    }
    if crate::memory::experience::sanitize::redact(text).0 != text {
        return Err(format!("{what} has something in it that looks like a secret"));
    }
    Ok(())
}

fn input_name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

fn validate_target(t: &Target, what: &str) -> Result<(), String> {
    if t.role.is_empty() || t.role.len() > 12 || !t.role.chars().all(|c| c.is_ascii_lowercase()) {
        return Err(format!("{what}: the role \"{}\" is not a plain role such as button or textbox", t.role));
    }
    text_ok(&format!("{what}: the name of the control"), &t.name, MAX_NAME_CHARS)?;
    if t.name != t.name.trim() {
        return Err(format!("{what}: the name of the control has spaces at its ends"));
    }
    Ok(())
}

fn validate_step(s: &Step, i: usize, d: &RecipeDraft) -> Result<(), String> {
    let n = i + 1;
    let what = format!("step {n}");
    text_ok(&format!("the note of {what}"), &s.note, MAX_LINE_CHARS)?;
    if let Some(t) = &s.target {
        validate_target(t, &what)?;
    }
    if (i == 0) != (s.action == Action::Open) {
        return Err(format!("{what}: the first step, and only the first, opens the start page"));
    }
    match s.action {
        Action::Open => {
            let url = s.url.as_deref().ok_or_else(|| format!("{what} opens nothing: it has no address"))?;
            let (_, host) = check_address(url).map_err(|e| format!("{what}: {e}"))?;
            if host != d.site {
                return Err(format!("{what} opens another site ({host}); a recipe stays on {}", d.site));
            }
            if url != d.start_url {
                return Err(format!("{what} opens a page other than the start address, and a draft opens only the start address"));
            }
            if s.target.is_some() || s.key.is_some() || s.value_from.is_some() || s.irreversible {
                return Err(format!("{what}: an open step takes only an address"));
            }
        }
        Action::Click => {
            let t = s.target.as_ref().ok_or_else(|| format!("{what} clicks nothing: it has no control"))?;
            if send_score(&norm(&t.name)).is_some() && !s.irreversible {
                return Err(format!("{what} presses \"{}\", which sends something, but it is not marked irreversible", t.name));
            }
        }
        Action::Type => {
            let t = s.target.as_ref().ok_or_else(|| format!("{what} types into nothing: it has no control"))?;
            if !matches!(t.role.as_str(), "textbox" | "searchbox" | "combobox") {
                return Err(format!("{what} types into a {}, which is not a place to write", t.role));
            }
            if !s.value_from.as_deref().is_some_and(input_name_ok) {
                return Err(format!("{what} types, but names no input to take the words from"));
            }
            if s.irreversible {
                return Err(format!("{what}: typing is not marked irreversible; only a step that sends is"));
            }
        }
        Action::Press => {
            let key = s.key.as_deref().ok_or_else(|| format!("{what} presses no key"))?;
            if !KEYS.contains(&key) {
                return Err(format!("{what} presses \"{key}\", and only Enter, Tab and Escape are allowed"));
            }
            if key == "Enter" && !s.irreversible {
                return Err(format!("{what} presses Enter, which can send, but it is not marked irreversible"));
            }
        }
        Action::WaitFor => {
            if s.target.is_none() {
                return Err(format!("{what} waits for nothing: it has no control"));
            }
        }
    }
    Ok(())
}

fn validate_post(p: &Post, j: usize, steps: usize) -> Result<(), String> {
    let what = format!("check {}", j + 1);
    if p.after_step >= steps {
        return Err(format!("{what} follows step {}, and there is no such step", p.after_step + 1));
    }
    match p.kind {
        PostKind::Appears | PostKind::Disappears => {
            let t = p.target.as_ref().ok_or_else(|| format!("{what} names no control"))?;
            validate_target(t, &what)?;
            if p.text.is_some() || p.text_from.is_some() {
                return Err(format!("{what} checks a control, not words"));
            }
        }
        PostKind::TextPresent => {
            if p.target.is_some() {
                return Err(format!("{what} checks words, not a control"));
            }
            match (&p.text, &p.text_from) {
                (Some(t), None) => text_ok(&format!("{what}: the words"), t, MAX_LINE_CHARS)?,
                (None, Some(k)) if input_name_ok(k) => {}
                _ => return Err(format!("{what} must name either the words or the input they come from, and only one")),
            }
        }
    }
    Ok(())
}
