//! Pre-approved project checks: the person allows, once and in their own hands, the tests and the build of one folder for one run, and the run does not stop at each of them (card
//! OA10-05; task WF-14 of the OpenCode audit; decision D-07 taken as the audit proposes and shipped off).
//!
//! What a pre-approval is, and is not:
//!
//! * It is **made by the person** with the app's token (`POST /api/v1/preapprovals`, UI token only; `aqua-cli preapprove` for a person at a terminal). It is never made by the API token,
//!   by a voice request or by a model, and the host holds it only in memory: a restart of Aqua forgets every one.
//! * It names **one folder**, **some kinds** of checks (`tests`, `build`, `lint`, `typecheck`), a **number of calls** (60 by default, 500 at most) and a **time** (30 minutes by default,
//!   12 hours at most). It is given to **one run** (`attach`, when the run is started with its id): another run, or the same id again, finds nothing.
//! * It answers **one question and no other**: the question the host would put for a project's own check (`cargo test`, `npm test`, `go test`... only the command lines the terminal tools
//!   already count as a project's checks, each an allowlist of its flags) run in the named folder, which the terminal tools trust. A call that is any other command, an edit, a request
//!   on the network, a call that always asks (a push, an install, a delete), a call a hook or a rule of `permissions.json` asks about, a call the run repeats after a restart, and a call
//!   made with the API token directly (no run) are not covered and ask as they always did. It can only take away a question that a clean run would be asked as an ordinary one (never one
//!   that always asks): see `ToolHost::use_preapproval`.
//! * Each use is **written down**: the audit ledger and the run's record say `preapproved` and by which pre-approval (its short form), so that the person can see afterwards what ran
//!   without asking.
//! * The person can **take it back** at any moment (`revoke`); the next check of the run asks again.
//!
//! The switch is `AQUA_PREAPPROVALS=on` of the core's environment (off unless set): while it is off no pre-approval can be made or attached, and none is used.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The environment variable that turns pre-approvals on.
pub const SWITCH_ENV: &str = "AQUA_PREAPPROVALS";
/// The kinds of check a pre-approval can name.
pub const KINDS: [&str; 4] = ["tests", "build", "lint", "typecheck"];
/// Calls of a run that a pre-approval answers, when the person does not say.
pub const DEFAULT_BUDGET: u32 = 60;
/// The most calls a pre-approval may answer.
pub const MAX_BUDGET: u32 = 500;
/// How long a pre-approval lasts, in minutes, when the person does not say.
pub const DEFAULT_MINUTES: u32 = 30;
/// The longest a pre-approval may last (twelve hours: an overnight run).
pub const MAX_MINUTES: u32 = 720;
/// How many pre-approvals can be open at once.
pub const MAX_OPEN: usize = 16;

/// Are pre-approvals on? Read on every call, like the other switches of the core.
pub fn enabled() -> bool {
    matches!(std::env::var(SWITCH_ENV).ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
}

/// A folder as it is compared: forward slashes, no `\\?\`, no trailing slash, and (on Windows, where the file system does not tell `Src` from `src`) in lower case.
pub fn normal_folder(path: &str) -> String {
    let s = path.trim().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s).trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

/// Is `inner` the folder `outer` or a folder inside it? Both are folders the file guard has resolved (real paths, no `..`).
pub fn folder_contains(outer: &str, inner: &str) -> bool {
    let (outer, inner) = (normal_folder(outer), normal_folder(inner));
    !outer.is_empty() && !outer.ends_with(':') && (inner == outer || inner.strip_prefix(&outer).is_some_and(|rest| rest.starts_with('/')))
}

/// What a call is, when it is one of a project's own checks: the kind (one of [`KINDS`]) and the folder it runs in. A provider says it (`ToolProvider::project_check`); the host never
/// reads the kind from what the model wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCheck {
    pub kind: &'static str,
    pub folder: String,
}

/// Where a pre-approval stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Made, and not yet given to a run.
    Open,
    /// Given to a run; its calls are being answered.
    InUse,
    /// Every call it could answer was used.
    Spent,
    /// Its time is over.
    Expired,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Open => "open",
            State::InUse => "in_use",
            State::Spent => "spent",
            State::Expired => "expired",
        }
    }
}

/// A pre-approval as the person sees it. Never holds anything but what they chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub id: String,
    pub folder: String,
    pub kinds: Vec<String>,
    pub budget: u32,
    pub used: u32,
    pub minutes: u32,
    pub expires_in_secs: u64,
    pub run_id: Option<String>,
    pub state: State,
}

struct Entry {
    id: String,
    folder: String,
    kinds: BTreeSet<String>,
    budget: u32,
    used: u32,
    minutes: u32,
    expires: Instant,
    run: Option<String>,
}

impl Entry {
    fn state(&self, now: Instant) -> State {
        if now >= self.expires {
            State::Expired
        } else if self.used >= self.budget {
            State::Spent
        } else if self.run.is_some() {
            State::InUse
        } else {
            State::Open
        }
    }

    fn view(&self, now: Instant) -> View {
        View {
            id: self.id.clone(),
            folder: self.folder.clone(),
            kinds: self.kinds.iter().cloned().collect(),
            budget: self.budget,
            used: self.used,
            minutes: self.minutes,
            expires_in_secs: self.expires.saturating_duration_since(now).as_secs(),
            run_id: self.run.clone(),
            state: self.state(now),
        }
    }
}

/// The short form of an id that the ledger and the run's record carry (`pa_` and eight characters): enough to tell which, not enough to use.
pub fn short(id: &str) -> String {
    id.chars().take(11).collect()
}

/// The open pre-approvals of one host.
#[derive(Default)]
pub struct Preapprovals {
    entries: Mutex<Vec<Entry>>,
}

fn new_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("the system's random numbers are not available ({e})"))?;
    Ok(format!("pa_{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

impl Preapprovals {
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes one. `folder` is a folder the file guard resolved. `kinds` are some of [`KINDS`] (at least one); `budget` and `minutes` are kept inside their bounds (`Err` for a value of
    /// zero or beyond: a number that was asked for and cannot be given is not quietly changed).
    pub fn create(&self, folder: &str, kinds: &[String], budget: Option<u32>, minutes: Option<u32>) -> Result<View, String> {
        if normal_folder(folder).is_empty() {
            return Err("name the folder".into());
        }
        let mut set = BTreeSet::new();
        for k in kinds {
            if !KINDS.contains(&k.as_str()) {
                return Err(format!("a kind of check is one of {}", KINDS.join(", ")));
            }
            set.insert(k.clone());
        }
        if set.is_empty() {
            return Err(format!("say which checks: some of {}", KINDS.join(", ")));
        }
        let budget = budget.unwrap_or(DEFAULT_BUDGET);
        if !(1..=MAX_BUDGET).contains(&budget) {
            return Err(format!("budget is a number of calls from 1 to {MAX_BUDGET}"));
        }
        let minutes = minutes.unwrap_or(DEFAULT_MINUTES);
        if !(1..=MAX_MINUTES).contains(&minutes) {
            return Err(format!("minutes is from 1 to {MAX_MINUTES}"));
        }
        let now = Instant::now();
        let mut entries = self.entries();
        // What is over is forgotten first; a person who made many and used none has to take some back.
        entries.retain(|e| !matches!(e.state(now), State::Expired | State::Spent));
        if entries.len() >= MAX_OPEN {
            return Err(format!("{MAX_OPEN} pre-approvals are open: take one back first"));
        }
        let entry = Entry { id: new_id()?, folder: folder.trim().to_string(), kinds: set, budget, used: 0, minutes, expires: now + Duration::from_secs(u64::from(minutes) * 60), run: None };
        let view = entry.view(now);
        entries.push(entry);
        Ok(view)
    }

    /// Could `id` be given to a run that works in `directory` now? `Err` says why not, for the person.
    pub fn check_attachable(&self, id: &str, directory: &str) -> Result<(), String> {
        let now = Instant::now();
        let entries = self.entries();
        let e = entries.iter().find(|e| e.id == id).ok_or("there is no such pre-approval (it was taken back, it ended, or Aqua was restarted)")?;
        match e.state(now) {
            State::Expired => return Err("that pre-approval has ended".into()),
            State::Spent => return Err("that pre-approval is used up".into()),
            State::InUse => return Err("that pre-approval is given to a run already: it covers one run".into()),
            State::Open => {}
        }
        if !folder_contains(&e.folder, directory) {
            return Err("the run works outside the folder the pre-approval names".into());
        }
        Ok(())
    }

    /// Gives `id` to run `run_id` (once). The checks of [`check_attachable`](Self::check_attachable) are made again, under the lock.
    pub fn attach(&self, id: &str, run_id: &str, directory: &str) -> Result<(), String> {
        let now = Instant::now();
        let mut entries = self.entries();
        let e = entries.iter_mut().find(|e| e.id == id).ok_or("there is no such pre-approval")?;
        if e.state(now) != State::Open {
            return Err("that pre-approval cannot be given to a run now".into());
        }
        if !folder_contains(&e.folder, directory) {
            return Err("the run works outside the folder the pre-approval names".into());
        }
        e.run = Some(run_id.to_string());
        Ok(())
    }

    /// Does this run have a pre-approval that can still answer? (A quick look before the host works anything out.)
    pub fn has_run(&self, run_id: &str) -> bool {
        let now = Instant::now();
        self.entries().iter().any(|e| e.run.as_deref() == Some(run_id) && e.state(now) == State::InUse)
    }

    /// Takes one answer for a check of `kind` run in `cwd` by run `run_id`: `Some(id)` when the run has a pre-approval that names the kind, holds the folder, is not over and has a call left;
    /// the call is counted. `None`: the question is put as it always was.
    pub fn take(&self, run_id: &str, kind: &str, cwd: &str) -> Option<String> {
        self.take_at(Instant::now(), run_id, kind, cwd)
    }

    fn take_at(&self, now: Instant, run_id: &str, kind: &str, cwd: &str) -> Option<String> {
        if run_id.is_empty() {
            return None;
        }
        let mut entries = self.entries();
        let e = entries.iter_mut().find(|e| e.run.as_deref() == Some(run_id))?;
        if e.state(now) != State::InUse || !e.kinds.contains(kind) || !folder_contains(&e.folder, cwd) {
            return None;
        }
        e.used += 1;
        Some(e.id.clone())
    }

    /// Takes it back: it answers nothing from now on. `true` when there was one.
    pub fn revoke(&self, id: &str) -> bool {
        let mut entries = self.entries();
        let before = entries.len();
        entries.retain(|e| e.id != id);
        entries.len() != before
    }

    /// The run ended: what was given to it is over with it.
    pub fn release_run(&self, run_id: &str) {
        self.entries().retain(|e| e.run.as_deref() != Some(run_id));
    }

    /// Every one that is open or in use or lately over, newest last.
    pub fn list(&self) -> Vec<View> {
        let now = Instant::now();
        self.entries().iter().map(|e| e.view(now)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(p: &Preapprovals) -> View {
        p.create("E:/work/project", &["tests".into(), "build".into()], Some(3), Some(30)).unwrap()
    }

    #[test]
    fn a_folder_holds_itself_and_what_is_inside_and_nothing_that_only_starts_like_it() {
        assert!(folder_contains("E:/work/project", "E:/work/project"));
        assert!(folder_contains("E:\\work\\project\\", "E:/work/project/crates/a"), "a back slash and a closing slash do not matter");
        assert_eq!(folder_contains("E:/work/project", "e:/WORK/project/crates/a"), cfg!(windows), "the case does not matter where the file system does not tell them apart");
        assert!(folder_contains("E:/work/project", "E:/work/project/crates/a"));
        assert!(!folder_contains("E:/work/project", "E:/work/project2"));
        assert!(!folder_contains("E:/work/project", "E:/work"));
        assert!(!folder_contains("E:/work/project", "D:/work/project"));
        assert!(!folder_contains("", "E:/work"));
        assert!(!folder_contains("E:", "E:/work"), "a drive is not a project folder");
        assert!(folder_contains("//?/E:/work/project", "E:/work/project/x"));
    }

    #[test]
    fn a_pre_approval_is_given_to_one_run_and_answers_that_runs_checks_in_its_folder() {
        let p = Preapprovals::new();
        let v = make(&p);
        assert_eq!((v.state, v.budget, v.used, v.minutes), (State::Open, 3, 0, 30));
        assert!(v.id.starts_with("pa_") && v.id.len() == 35, "{}", v.id);
        // Not given to a run: it answers nothing.
        assert_eq!(p.take("run_1", "tests", "E:/work/project"), None);
        p.check_attachable(&v.id, "E:/work/project/crates/a").unwrap();
        p.attach(&v.id, "run_1", "E:/work/project").unwrap();
        assert!(p.has_run("run_1") && !p.has_run("run_2"));
        assert_eq!(p.take("run_1", "tests", "E:/work/project/crates/a").as_deref(), Some(v.id.as_str()));
        assert_eq!(p.take("run_1", "build", "E:/work/project").as_deref(), Some(v.id.as_str()));
        // Another run, another kind and another folder find nothing.
        assert_eq!(p.take("run_2", "tests", "E:/work/project"), None);
        assert_eq!(p.take("run_1", "lint", "E:/work/project"), None, "a kind that was not named");
        assert_eq!(p.take("run_1", "tests", "E:/work/other"), None, "a folder that was not named");
        assert_eq!(p.take("", "tests", "E:/work/project"), None);
        // The third answers, the fourth does not: the budget is the person's limit.
        assert!(p.take("run_1", "tests", "E:/work/project").is_some());
        assert_eq!(p.take("run_1", "tests", "E:/work/project"), None);
        assert_eq!(p.list()[0].state, State::Spent);
        assert_eq!(p.list()[0].used, 3);
    }

    #[test]
    fn a_pre_approval_cannot_be_given_twice_or_to_a_run_outside_its_folder() {
        let p = Preapprovals::new();
        let v = make(&p);
        assert!(p.check_attachable(&v.id, "E:/work/elsewhere").unwrap_err().contains("outside the folder"));
        assert!(p.attach(&v.id, "run_1", "E:/work/elsewhere").is_err());
        assert!(!p.has_run("run_1"));
        p.attach(&v.id, "run_1", "E:/work/project").unwrap();
        assert!(p.check_attachable(&v.id, "E:/work/project").unwrap_err().contains("one run"));
        assert!(p.attach(&v.id, "run_2", "E:/work/project").is_err(), "the second run finds it taken");
        assert!(p.check_attachable("pa_nothing", "E:/work/project").is_err());
        assert_eq!(p.take("run_2", "tests", "E:/work/project"), None);
    }

    #[test]
    fn what_was_taken_back_or_ended_answers_nothing() {
        let p = Preapprovals::new();
        let v = make(&p);
        p.attach(&v.id, "run_1", "E:/work/project").unwrap();
        assert!(p.revoke(&v.id));
        assert!(!p.revoke(&v.id));
        assert_eq!(p.take("run_1", "tests", "E:/work/project"), None, "the next check asks again");
        // Its time.
        let w = make(&p);
        p.attach(&w.id, "run_2", "E:/work/project").unwrap();
        let later = Instant::now() + Duration::from_secs(31 * 60);
        assert_eq!(p.take_at(later, "run_2", "tests", "E:/work/project"), None);
        assert!(p.take_at(Instant::now(), "run_2", "tests", "E:/work/project").is_some());
        // The run's end.
        p.release_run("run_2");
        assert!(p.list().is_empty() || p.list().iter().all(|v| v.run_id.as_deref() != Some("run_2")));
        assert_eq!(p.take("run_2", "tests", "E:/work/project"), None);
    }

    #[test]
    fn the_numbers_are_the_persons_and_a_number_that_cannot_be_given_is_refused_not_changed() {
        let p = Preapprovals::new();
        let kinds: Vec<String> = vec!["tests".into()];
        for (budget, minutes) in [(Some(0), None), (Some(501), None), (None, Some(0)), (None, Some(721))] {
            assert!(p.create("E:/work/project", &kinds, budget, minutes).is_err(), "{budget:?} {minutes:?}");
        }
        assert!(p.create("E:/work/project", &[], None, None).is_err());
        assert!(p.create("E:/work/project", &["deploy".into()], None, None).is_err());
        assert!(p.create("", &kinds, None, None).is_err());
        let v = p.create("E:/work/project", &kinds, None, None).unwrap();
        assert_eq!((v.budget, v.minutes), (DEFAULT_BUDGET, DEFAULT_MINUTES));
        // Only so many are open at once.
        for _ in 1..MAX_OPEN {
            p.create("E:/work/project", &kinds, None, None).unwrap();
        }
        assert!(p.create("E:/work/project", &kinds, None, None).unwrap_err().contains("take one back"));
    }

    #[test]
    fn the_short_form_is_all_the_ledger_carries() {
        let id = new_id().unwrap();
        assert_eq!(short(&id).len(), 11);
        assert!(id.starts_with(&short(&id)));
        assert_ne!(id, new_id().unwrap(), "ids are random");
    }
}
