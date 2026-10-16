//! The site recipe builder (card R7-10): draft a recipe for a site that has none, from what its page shows, and rehearse the draft on a stand-in page. It never acts on a real site.
//!
//! Three parts, each small and each unable to do more than its job:
//! - `explore` reads a page through `ReadOnlyPage`, a trait that has no way to click, type or press. It lists the controls that look like a way to write and send a text, and
//!   any wall (a sign-in form, a check) the page puts up. Nothing is drafted past a wall.
//! - `draft` turns that list into a `RecipeDraft`: data only. Steps by role and name, what to check afterwards, the risks, and what only the person can do. A step that sends
//!   something is marked irreversible, and the last line for the person always says the recipe has never been tried on the real site.
//! - `rehearse` runs a draft against a stand-in page (`RehearsalPage`) and says which step failed. It refuses a draft that sends something with no check that it worked. A
//!   rehearsal on a stand-in never shows that the site works, and its result says so.
//!
//! No model is asked anything here: word lists decide (a model could later rank the candidates, and could never be given more than the data in `Exploration`). The text on a page
//! is data: it can become the name of a control in a draft, and nothing more. A draft becomes a proposal the person reads (`self_repair::proposals`); approving it only keeps the
//! draft as a file (`drafts`) and never runs it. See `documentation/RECIPE_BUILDER.md`.

pub mod draft;
pub mod drafts;
pub mod explore;
pub mod rehearse;

pub use draft::{build_draft, Action, Post, PostKind, RecipeDraft, Step, Target, EXPECTED_NOTE, INPUT_NAME, NEVER_TRIED};
pub use explore::{explore, Candidate, CandidateKind, Exploration, ExploreError, PageView, ReadOnlyPage, Wall, WallKind};
pub use rehearse::{default_inputs, rehearse, Check, Rehearsal, RehearsalPage, StepResult, STAND_IN_NOTE};

#[cfg(test)]
mod tests;

/// Pages read while exploring (the first one and the targeted looks together).
pub const MAX_SNAPSHOTS: usize = 6;
/// Characters of one page that are read.
pub const MAX_PAGE_CHARS: usize = 60_000;
/// Controls taken from one page.
pub const MAX_ELEMENTS_PER_PAGE: usize = 400;
/// Candidates handed on (at most four of each kind).
pub const MAX_CANDIDATES: usize = 12;
/// Characters of a control's name that are kept.
pub const MAX_NAME_CHARS: usize = 120;
/// Characters of the goal that are kept.
pub const MAX_GOAL_CHARS: usize = 200;
/// Characters of an address.
pub const MAX_URL_CHARS: usize = 300;
/// Steps, checks and notes of one draft.
pub const MAX_STEPS: usize = 12;
pub const MAX_POSTS: usize = 6;
pub const MAX_NOTES: usize = 8;
/// Characters of one note or reason.
pub const MAX_LINE_CHARS: usize = 400;

/// A text as the vocabulary sees it: lower case, one kind of apostrophe, single spaces. Nothing is trimmed from the ends but white space, so "Post..." is not "Post".
pub(crate) fn norm(text: &str) -> String {
    let lower: String = text.to_lowercase().chars().map(|c| if c == '\u{2019}' || c == '\u{2018}' { '\'' } else { c }).collect();
    lower.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The words of a normalised text.
pub(crate) fn words(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\'')).filter(|w| !w.is_empty()).collect()
}

/// One line of text from outside, made safe to keep: secrets scrubbed, control characters and runs of white space gone, cut to `max` characters (with "..." when it was cut).
pub(crate) fn clean_line(text: &str, max: usize) -> String {
    let (scrubbed, _) = crate::memory::experience::sanitize::redact(text);
    let spaced: String = scrubbed.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let line = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        line
    } else {
        format!("{}...", line.chars().take(max.saturating_sub(3)).collect::<String>().trim_end())
    }
}

/// The name of a control, made safe to keep like `clean_line` but cut without "...": a name that is cut still has to be found in the page (a prefix of the real name is).
pub(crate) fn clean_name(text: &str, max: usize) -> String {
    let (scrubbed, _) = crate::memory::experience::sanitize::redact(text);
    let spaced: String = scrubbed.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let line = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    line.chars().take(max).collect::<String>().trim_end().to_string()
}
