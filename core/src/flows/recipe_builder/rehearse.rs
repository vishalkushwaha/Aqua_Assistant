//! Rehearsing a draft on a stand-in page (card R7-10).
//!
//! A `RehearsalPage` is a page kept in memory or a recorded one. Nothing in this card implements it over the real browser, and a rehearsal never counts as the site working: the
//! result says "rehearsed on a stand-in only". What it does show is that the draft hangs together: each control is found by role and name with the same matching the recipes use
//! (`browser_exec::pick`), a step that sends presses a button named exactly as the draft says (a button that only starts with the same word is not pressed), and the checks that
//! follow the send hold. A draft that sends something with no check that it worked is refused before a single step runs.

use super::draft::{Action, Post, PostKind, RecipeDraft, Step};
use super::norm;
use crate::flows::browser_exec::{describe, elements, pick, Element, Found};
use serde::Serialize;
use std::collections::BTreeMap;

/// What every rehearsal says about itself.
pub const STAND_IN_NOTE: &str = "Rehearsed on a stand-in page only. This shows the draft hangs together; it does not show that the real site works.";
/// The words typed in a rehearsal, when the caller gives none.
pub const SAMPLE_TEXT: &str = "A rehearsal post written by Aqua, not a real one";

/// A stand-in for the site. Each method does one thing a person could do on the page and says why it could not.
pub trait RehearsalPage {
    fn open(&mut self, url: &str) -> Result<(), String>;
    /// The page as text, lines such as `- button "Publish" [ref=e5]`.
    fn snapshot(&mut self) -> Result<String, String>;
    fn click(&mut self, role: &str, name: &str) -> Result<(), String>;
    fn type_into(&mut self, role: &str, name: &str, text: &str) -> Result<(), String>;
    fn press(&mut self, key: &str) -> Result<(), String>;
    fn wait_for(&mut self, text: &str) -> Result<(), String>;
}

/// One step as it ran. `step` counts from 1.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepResult {
    pub step: usize,
    pub what: String,
    pub ok: bool,
    pub detail: String,
}

/// One check that followed a step. `after_step` counts from 1.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub after_step: usize,
    pub what: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rehearsal {
    pub passed: bool,
    /// Why nothing was run, when the draft was refused.
    pub refused: Option<String>,
    pub steps: Vec<StepResult>,
    pub checks: Vec<Check>,
    /// The first step or check that failed, in words for the person.
    pub first_failure: Option<String>,
    /// Always `STAND_IN_NOTE`.
    pub note: String,
}

impl Rehearsal {
    /// The result as text for the person.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        if let Some(why) = &self.refused {
            s.push_str(&format!("Refused to rehearse: {why}\n"));
        } else if self.passed {
            s.push_str("Rehearsal passed.\n");
        } else {
            s.push_str(&format!("Rehearsal failed: {}\n", self.first_failure.as_deref().unwrap_or("")));
        }
        for r in &self.steps {
            s.push_str(&format!("  {}. {} - {}{}\n", r.step, r.what, if r.ok { "ok: " } else { "FAILED: " }, r.detail));
        }
        for c in &self.checks {
            s.push_str(&format!("  after step {}: {} - {}: {}\n", c.after_step, c.what, if c.ok { "ok" } else { "FAILED" }, c.detail));
        }
        s.push_str(&self.note);
        s.push('\n');
        s
    }
}

/// The sample words for every input a draft types from.
pub fn default_inputs(draft: &RecipeDraft) -> BTreeMap<String, String> {
    draft.steps.iter().filter_map(|s| s.value_from.clone()).map(|k| (k, SAMPLE_TEXT.to_string())).collect()
}

/// The control with this role and name on the snapshot, found the way the recipes find one. The error says what is there instead.
pub fn locate(snapshot: &str, role: &str, name: &str) -> Result<Element, String> {
    match pick(&format!("{role}:{name}"), &elements(snapshot)) {
        Found::One(e) => Ok(e),
        Found::Ambiguous(list) => Err(format!("more than one control matches {role} \"{name}\" ({})", describe(&list))),
        Found::Missing(near) => Err(format!("no {role} named \"{name}\" is on the page{}", if near.is_empty() { ". The page shows nothing like it".to_string() } else { format!(". Similar controls: {}", describe(&near)) })),
    }
}

/// The first words of a text as they would show on a page. Words too few to look for (under 12 characters) give nothing: a short phrase could match anything.
fn snippet(text: &str) -> Option<String> {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let s: String = norm(first).chars().take(40).collect();
    (s.chars().count() >= 12).then_some(s)
}

/// How many lines of the page show the snippet, not counting a text box (what is typed in a box is a draft, not a post).
fn shown_count(page_text: &str, snippet: &str) -> usize {
    page_text.lines().filter(|l| !l.trim_start().trim_start_matches("- ").starts_with("textbox") && norm(l).contains(snippet)).count()
}

fn run_step(step: &Step, page: &mut dyn RehearsalPage, inputs: &BTreeMap<String, String>) -> Result<String, String> {
    match step.action {
        Action::Open => {
            let url = step.url.as_deref().ok_or("the step has no address")?;
            page.open(url)?;
            Ok(format!("opened {url}"))
        }
        Action::Click => {
            let t = step.target.as_ref().ok_or("the step has no control")?;
            let snap = page.snapshot().map_err(|e| format!("could not read the page: {e}"))?;
            let el = locate(&snap, &t.role, &t.name)?;
            // The step that sends presses a button named exactly as the draft says: "Posting..." and "Post settings" are not "Post".
            if step.irreversible && !el.name.trim().eq_ignore_ascii_case(t.name.trim()) {
                return Err(format!("the nearest {} is \"{}\", not \"{}\". Nothing was pressed", el.role, el.name, t.name));
            }
            page.click(&el.role, &el.name)?;
            Ok(format!("clicked {} \"{}\"", el.role, el.name))
        }
        Action::Type => {
            let t = step.target.as_ref().ok_or("the step has no control")?;
            let key = step.value_from.as_deref().ok_or("the step names no input")?;
            let text = inputs.get(key).ok_or_else(|| format!("there is no sample for the input \"{key}\""))?;
            let snap = page.snapshot().map_err(|e| format!("could not read the page: {e}"))?;
            let el = locate(&snap, &t.role, &t.name)?;
            page.type_into(&el.role, &el.name, text)?;
            // What was typed is not repeated: it may be private.
            Ok(format!("typed {} characters into {} \"{}\"", text.chars().count(), el.role, el.name))
        }
        Action::Press => {
            let key = step.key.as_deref().ok_or("the step presses no key")?;
            page.press(key)?;
            Ok(format!("pressed {key}"))
        }
        Action::WaitFor => {
            let t = step.target.as_ref().ok_or("the step has no control")?;
            page.wait_for(&t.name)?;
            Ok(format!("waited for {} \"{}\"", t.role, t.name))
        }
    }
}

/// Does the page show a control with this role and name (once or more)?
fn on_page(snapshot: &str, role: &str, name: &str) -> bool {
    !matches!(pick(&format!("{role}:{name}"), &elements(snapshot)), Found::Missing(_))
}

/// One check, after its step. `before` is the page as it was before that step (only needed for the words).
fn run_check(p: &Post, page: &mut dyn RehearsalPage, inputs: &BTreeMap<String, String>, before: Option<&str>) -> (bool, String) {
    let snap = match page.snapshot() {
        Ok(s) => s,
        Err(e) => return (false, format!("could not read the page: {e}")),
    };
    match p.kind {
        PostKind::Appears | PostKind::Disappears => {
            let Some(t) = &p.target else { return (false, "the check names no control".to_string()) };
            let here = on_page(&snap, &t.role, &t.name);
            match (p.kind, here) {
                (PostKind::Appears, true) => (true, "it is on the page".to_string()),
                (PostKind::Appears, false) => (false, "it is not on the page".to_string()),
                (_, false) => (true, "it is gone from the page".to_string()),
                (_, true) => (false, "it is still on the page".to_string()),
            }
        }
        PostKind::TextPresent => {
            let words = match (&p.text, &p.text_from) {
                (Some(t), _) => t.clone(),
                (None, Some(k)) => inputs.get(k).cloned().unwrap_or_default(),
                _ => String::new(),
            };
            let Some(needle) = snippet(&words) else { return (false, "the words are too short to look for on a page".to_string()) };
            let Some(before) = before else { return (false, "the page could not be read before the step, so new words cannot be told from old ones".to_string()) };
            let (was, now) = (shown_count(before, &needle), shown_count(&snap, &needle));
            if now > was {
                (true, format!("the words show {now} times now and showed {was} times before"))
            } else {
                (false, format!("the words show {now} times now and showed {was} times before, so nothing new is shown"))
            }
        }
    }
}

/// Rehearse a draft on a stand-in page with these sample inputs. The draft is checked first (`RecipeDraft::validate`): one that sends something with no check that it worked, or
/// that breaks another rule, is refused and no step runs. The first step or check that fails ends the rehearsal and is named. `passed` means every step and check held on the
/// stand-in, and nothing more.
pub fn rehearse(draft: &RecipeDraft, page: &mut dyn RehearsalPage, inputs: &BTreeMap<String, String>) -> Rehearsal {
    let mut r = Rehearsal { passed: false, refused: None, steps: Vec::new(), checks: Vec::new(), first_failure: None, note: STAND_IN_NOTE.to_string() };
    let refusal = match draft.validate() {
        Err(why) => Some(why),
        Ok(()) if draft.steps.is_empty() => Some(format!("the draft has no steps to rehearse{}", draft.empty_reason.as_ref().map(|x| format!(" ({x})")).unwrap_or_default())),
        Ok(()) => None,
    };
    if let Some(why) = refusal {
        r.first_failure = Some(why.clone());
        r.refused = Some(why);
        return r;
    }
    for (i, step) in draft.steps.iter().enumerate() {
        let n = i + 1;
        let what = step.describe_plain();
        let wants_words = draft.postconditions.iter().any(|p| p.after_step == i && p.kind == PostKind::TextPresent);
        let before = if wants_words { page.snapshot().ok() } else { None };
        match run_step(step, page, inputs) {
            Ok(detail) => r.steps.push(StepResult { step: n, what: what.clone(), ok: true, detail }),
            Err(e) => {
                r.first_failure = Some(format!("step {n} ({what}): {e}"));
                r.steps.push(StepResult { step: n, what, ok: false, detail: e });
                return r;
            }
        }
        for p in draft.postconditions.iter().filter(|p| p.after_step == i) {
            let (ok, detail) = run_check(p, page, inputs, before.as_deref());
            let label = p.describe();
            r.checks.push(Check { after_step: n, what: label.clone(), ok, detail: detail.clone() });
            if !ok {
                r.first_failure = Some(format!("step {n} ({what}) went through, but a check after it failed: {label}: {detail}"));
                return r;
            }
        }
    }
    r.passed = true;
    r
}
