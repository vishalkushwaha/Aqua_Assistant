//! The run-scoped edit grant: after the person has said yes, in the app, to ONE edit of a file, later edits of files in the same folder go ahead without a question until the run ends
//! (card OA6-05; task ED-10 of the OpenCode audit; decision D-08, answered 2026-10-11: built, and shipped off).
//!
//! What a grant is, and is not:
//!
//! * It is **made by the person, in the app**: the answer to the question about the first edit carries the run option (`ApprovalScope::Run`), and the route that takes the answer needs the
//!   UI token. It is never made by the API token, by a voice request, by a model's own words, by a project pack or by a hook, and a host that is not told so by its approver makes none.
//! * It names **one folder**: the folder of the file the person was shown the diff of, as the file guard resolved it (a real path: no `..`, no link or junction followed out of it). Sub-folders
//!   of that folder are covered; a sibling, a parent and a folder whose name only starts the same are not. It can never be widened: a run holds one grant, and a second answer for another
//!   folder makes none.
//! * It lives **only in this memory, for one run**: it is not written to disk, not shared between runs, not given to a helper agent, and it is gone when the run ends, when the run is stopped
//!   and after two hours at the most (the longest a run may last).
//! * It answers **one question and no other**: the question the host would put for a plain edit of a text file inside a trusted folder (`fs.edit`, `fs.write`, `fs.patch` that only change or
//!   create files), when the only reason it asks is that the run has read files of the project. It never answers a question about a delete, a move, a command, git, a secret file, a build or
//!   script file, a name the file tools never write (`.vscode`, `.cargo`, `.husky`...: those are refused before any question), or anything that always asks for another reason.
//! * It is **suspended for good the moment the run reads text from outside the project** (a web page, a mail, a tool's result that is not known to be local, a file outside the project
//!   folder): from then on every edit asks again, grant or not. Reading files inside the project does not suspend it.
//! * Every creation, every use and every suspension is **written down** in the audit ledger, with the folder and the run.
//!
//! The switch is `AQUA_EDIT_GRANT=on` of the core's environment (off unless set). While it is off no question offers the choice, no answer makes a grant, and the host decides exactly as it
//! did before this card.

use crate::preapproval::{folder_contains, normal_folder};
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The environment variable that turns the edit grant on.
pub const SWITCH_ENV: &str = "AQUA_EDIT_GRANT";

/// The most a grant lasts, whatever the run does: the longest a run may last (`max_seconds` of the autonomy setting is at most 7,200).
pub const MAX_AGE: Duration = Duration::from_secs(2 * 60 * 60);

/// How many runs may hold a grant at once. A run that ends gives its grant back, so this is a bound on a leak, not a limit anyone meets.
pub const MAX_HELD: usize = 64;

/// The tools a grant can cover: the ones that change or create the text of files. Nothing else (a delete, a move, a copy, a command, git) is ever covered.
pub const EDIT_TOOLS: [&str; 3] = ["fs.edit", "fs.write", "fs.patch"];

/// Is the edit grant on? Read on every call, like the other switches of the core.
pub fn enabled() -> bool {
    matches!(std::env::var(SWITCH_ENV).ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
}

/// A folder as the file guard hands it over names no `..` and no `.` step: a text that has one is not a resolved folder and is never compared as if it were.
fn is_resolved(folder: &str) -> bool {
    !folder.trim().is_empty() && !folder.replace('\\', "/").split('/').any(|seg| seg == ".." || seg == ".")
}

/// Is `tool` one a grant can cover?
pub fn is_edit_tool(tool: &str) -> bool {
    EDIT_TOOLS.contains(&tool)
}

/// What a file edit would change, folder by folder (`ToolProvider::edit_scope`): the folder that holds each file the call writes, as the file guard resolved it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditScope {
    pub folders: Vec<String>,
}

impl EditScope {
    /// The folders without a repeat, nothing empty, at most the first 20 (a patch changes 20 files at the most).
    pub fn new(folders: impl IntoIterator<Item = String>) -> Self {
        let mut out: Vec<String> = Vec::new();
        for f in folders {
            if f.trim().is_empty() || out.iter().any(|o| normal_folder(o) == normal_folder(&f)) {
                continue;
            }
            out.push(f);
            if out.len() == 20 {
                break;
            }
        }
        Self { folders: out }
    }

    /// The one folder every file is in, when there is exactly one.
    pub fn single(&self) -> Option<&str> {
        match self.folders.as_slice() {
            [one] => Some(one.as_str()),
            _ => None,
        }
    }
}

/// A grant as the person (and a test) sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantView {
    pub run: String,
    pub folder: String,
    /// How many calls it has let through.
    pub uses: u32,
    /// `Some(why)` once the run read text from outside the project: the grant answers nothing from then on.
    pub suspended: Option<String>,
    pub age_secs: u64,
}

/// What a run's grant says about a call that changes files in `folders`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cover {
    /// The run holds no grant (or it has run out).
    NoGrant,
    /// A grant names the folder of every file of the call.
    Covers { folder: String },
    /// The run holds a grant, and it does not name the folder of at least one of the files.
    Elsewhere { folder: String },
    /// The run holds a grant, and the run has read text from outside the project since.
    Suspended { folder: String, why: String },
}

struct Entry {
    folder: String,
    made: Instant,
    uses: u32,
    suspended: Option<String>,
}

impl Entry {
    fn view(&self, run: &str, now: Instant) -> GrantView {
        GrantView { run: run.to_string(), folder: self.folder.clone(), uses: self.uses, suspended: self.suspended.clone(), age_secs: now.saturating_duration_since(self.made).as_secs() }
    }

    fn over(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.made) >= MAX_AGE
    }
}

/// The grants of the runs of one host. Memory only.
#[derive(Default)]
pub struct RunGrants {
    entries: Mutex<HashMap<String, Entry>>,
}

impl RunGrants {
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// May run `run` be given a grant for `folder` now? `Err` says why not. Nothing is changed.
    pub fn can_create(&self, run: &str, folder: &str) -> Result<(), String> {
        self.check_create(Instant::now(), run, folder, &self.entries())
    }

    fn check_create(&self, now: Instant, run: &str, folder: &str, entries: &HashMap<String, Entry>) -> Result<(), String> {
        if run.trim().is_empty() {
            return Err("a grant belongs to a run, and this call has none".into());
        }
        let key = normal_folder(folder);
        // A drive, a share or an empty name is no folder of a project: a grant never names the top of a disk. Nor does a text that still has a `..` in it.
        if key.is_empty() || key.ends_with(':') || !key.contains('/') || !is_resolved(folder) {
            return Err("a drive or the top of a share is not a folder a grant may name".into());
        }
        match entries.get(run).filter(|e| !e.over(now)) {
            Some(e) if normal_folder(&e.folder) == key => Ok(()),
            Some(e) => Err(format!("this run already holds a grant for {}: a run holds one folder, and it cannot be widened", e.folder)),
            None if entries.values().filter(|e| !e.over(now)).count() >= MAX_HELD => Err("too many runs hold a grant at once".into()),
            None => Ok(()),
        }
    }

    /// Gives `run` a grant for `folder` (a folder the file guard resolved). `Ok` with the grant when it holds already for the same folder (nothing changes); `Err` when it would be a
    /// second folder, or no folder a grant may name.
    pub fn create(&self, run: &str, folder: &str) -> Result<GrantView, String> {
        self.create_at(Instant::now(), run, folder)
    }

    fn create_at(&self, now: Instant, run: &str, folder: &str) -> Result<GrantView, String> {
        let mut entries = self.entries();
        entries.retain(|_, e| !e.over(now));
        self.check_create(now, run, folder, &entries)?;
        let entry = entries.entry(run.to_string()).or_insert_with(|| Entry { folder: folder.trim().to_string(), made: now, uses: 0, suspended: None });
        Ok(entry.view(run, now))
    }

    /// What the grant of `run` says about a call that changes files in `folders`.
    pub fn check(&self, run: &str, folders: &[String]) -> Cover {
        self.check_at(Instant::now(), run, folders)
    }

    fn check_at(&self, now: Instant, run: &str, folders: &[String]) -> Cover {
        let mut entries = self.entries();
        if entries.get(run).is_some_and(|e| e.over(now)) {
            entries.remove(run);
        }
        let Some(e) = entries.get(run) else { return Cover::NoGrant };
        if let Some(why) = &e.suspended {
            return Cover::Suspended { folder: e.folder.clone(), why: why.clone() };
        }
        if !folders.is_empty() && folders.iter().all(|f| is_resolved(f) && folder_contains(&e.folder, f)) {
            Cover::Covers { folder: e.folder.clone() }
        } else {
            Cover::Elsewhere { folder: e.folder.clone() }
        }
    }

    /// A call went through on the grant of `run`: the number of calls it has let through, this one included.
    pub fn note_use(&self, run: &str) -> u32 {
        match self.entries().get_mut(run) {
            Some(e) => {
                e.uses += 1;
                e.uses
            }
            None => 0,
        }
    }

    /// The run read text from outside the project: its grant answers nothing from now on, for the rest of the run. `Some(grant)` the first time, when there was one to suspend.
    pub fn suspend(&self, run: &str, why: &str) -> Option<GrantView> {
        let mut entries = self.entries();
        let e = entries.get_mut(run)?;
        if e.suspended.is_some() {
            return None;
        }
        e.suspended = Some(why.to_string());
        Some(e.view(run, Instant::now()))
    }

    /// The run ended or was stopped: its grant goes with it. `Some(grant)` when there was one.
    pub fn release_run(&self, run: &str) -> Option<GrantView> {
        self.entries().remove(run).map(|e| e.view(run, Instant::now()))
    }

    /// The grant of `run`, if it holds one that has not run out.
    pub fn view(&self, run: &str) -> Option<GrantView> {
        let now = Instant::now();
        self.entries().get(run).filter(|e| !e.over(now)).map(|e| e.view(run, now))
    }

    /// How many runs hold a grant.
    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "E:/work/project";

    fn src() -> String {
        format!("{PROJECT}/src")
    }

    #[test]
    fn a_grant_covers_its_folder_and_what_is_inside_it_and_nothing_that_only_starts_like_it() {
        let g = RunGrants::new();
        g.create("run_1", &src()).unwrap();
        let covers = |folders: &[&str]| g.check("run_1", &folders.iter().map(|f| f.to_string()).collect::<Vec<_>>());
        assert_eq!(covers(&[&src()]), Cover::Covers { folder: src() });
        assert_eq!(covers(&[&format!("{PROJECT}/src/deep/er")]), Cover::Covers { folder: src() }, "a sub-folder is inside");
        assert_eq!(covers(&[&format!("{PROJECT}/src2")]), Cover::Elsewhere { folder: src() }, "a sibling that only starts the same is not inside");
        assert_eq!(covers(&[&format!("{PROJECT}/srcs")]), Cover::Elsewhere { folder: src() });
        assert_eq!(covers(&[PROJECT]), Cover::Elsewhere { folder: src() }, "the parent is not covered");
        assert_eq!(covers(&[&format!("{PROJECT}/docs")]), Cover::Elsewhere { folder: src() }, "a sibling is not covered");
        assert_eq!(covers(&["E:/work"]), Cover::Elsewhere { folder: src() });
        assert_eq!(covers(&["D:/work/project/src"]), Cover::Elsewhere { folder: src() }, "another drive");
        assert_eq!(covers(&[&src(), &format!("{PROJECT}/docs")]), Cover::Elsewhere { folder: src() }, "one file outside the folder and the whole call asks");
        assert_eq!(covers(&[]), Cover::Elsewhere { folder: src() }, "a call that names no folder is not covered");
        assert_eq!(covers(&[&format!("{PROJECT}/src/../docs")]), Cover::Elsewhere { folder: src() }, "a path that is not the resolved one does not pass for one");
        assert_eq!(g.check("run_2", &[src()]), Cover::NoGrant, "another run holds nothing");
        assert_eq!(g.check("", &[src()]), Cover::NoGrant);
    }

    #[test]
    fn the_case_of_a_folder_does_not_matter_where_the_file_system_does_not_tell_them_apart() {
        let g = RunGrants::new();
        g.create("run_1", "E:/Work/Project/Src").unwrap();
        let again = g.check("run_1", &["e:/work/project/src/lib".to_string()]);
        if cfg!(windows) {
            assert!(matches!(again, Cover::Covers { .. }), "{again:?}");
        } else {
            assert!(matches!(again, Cover::Elsewhere { .. }), "{again:?}");
        }
        let back = g.check("run_1", &["E:\\Work\\Project\\Src\\lib".to_string()]);
        assert!(matches!(back, Cover::Covers { .. }), "a back slash is a slash: {back:?}");
    }

    #[test]
    fn a_run_holds_one_folder_and_cannot_widen_it() {
        let g = RunGrants::new();
        let made = g.create("run_1", &src()).unwrap();
        assert_eq!((made.folder.as_str(), made.uses, made.suspended.clone()), (src().as_str(), 0, None));
        // The same folder again changes nothing; any other is refused, the parent above all.
        assert!(g.create("run_1", &src()).is_ok());
        for other in [PROJECT.to_string(), format!("{PROJECT}/docs"), format!("{PROJECT}/src/deep"), "E:/work".to_string()] {
            let why = g.create("run_1", &other).unwrap_err();
            assert!(why.contains("one folder") && why.contains("cannot be widened"), "{other}: {why}");
            assert!(g.can_create("run_1", &other).is_err());
        }
        assert_eq!(g.view("run_1").unwrap().folder, src(), "what was refused changed nothing");
        // Another run is a grant of its own.
        assert!(g.can_create("run_2", &format!("{PROJECT}/docs")).is_ok());
        g.create("run_2", &format!("{PROJECT}/docs")).unwrap();
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn a_drive_a_share_an_empty_name_and_a_run_that_has_no_id_get_no_grant() {
        let g = RunGrants::new();
        for folder in ["", "   ", "E:", "E:/", "E:\\", "/", "//?/E:/"] {
            assert!(g.create("run_1", folder).is_err(), "{folder:?}");
        }
        assert!(g.create("", &src()).is_err());
        assert!(g.create("   ", &src()).is_err());
        assert!(g.is_empty());
    }

    #[test]
    fn a_suspended_grant_answers_nothing_for_the_rest_of_the_run_and_is_not_made_again() {
        let g = RunGrants::new();
        g.create("run_1", &src()).unwrap();
        assert_eq!(g.note_use("run_1"), 1);
        assert_eq!(g.note_use("run_1"), 2);
        let first = g.suspend("run_1", "the run read a web page").unwrap();
        assert_eq!((first.uses, first.suspended.as_deref()), (2, Some("the run read a web page")));
        assert!(g.suspend("run_1", "and then a mail").is_none(), "only the first suspension is news");
        assert_eq!(g.check("run_1", &[src()]), Cover::Suspended { folder: src(), why: "the run read a web page".into() });
        // Nothing lifts it, and the same folder does not get a fresh one.
        assert!(g.create("run_1", &src()).is_ok(), "asking again for the same folder is not an error");
        assert!(matches!(g.check("run_1", &[src()]), Cover::Suspended { .. }), "and it does not lift the suspension");
        assert!(g.suspend("run_9", "nothing to suspend").is_none());
    }

    #[test]
    fn a_grant_ends_with_its_run_and_the_next_run_finds_nothing() {
        let g = RunGrants::new();
        g.create("run_1", &src()).unwrap();
        let gone = g.release_run("run_1").unwrap();
        assert_eq!(gone.folder, src());
        assert!(g.release_run("run_1").is_none());
        assert_eq!(g.check("run_1", &[src()]), Cover::NoGrant);
        assert!(g.view("run_1").is_none() && g.is_empty());
        assert_eq!(g.note_use("run_1"), 0);
    }

    #[test]
    fn a_grant_runs_out_after_the_longest_a_run_may_last() {
        let g = RunGrants::new();
        let start = Instant::now();
        g.create_at(start, "run_1", &src()).unwrap();
        assert!(matches!(g.check_at(start + MAX_AGE - Duration::from_secs(1), "run_1", &[src()]), Cover::Covers { .. }));
        assert_eq!(g.check_at(start + MAX_AGE, "run_1", &[src()]), Cover::NoGrant);
        assert!(g.is_empty(), "what ran out is forgotten");
        // A run that ran out can be given one again, as a new grant with no uses.
        g.create_at(start, "run_2", &src()).unwrap();
        let later = start + MAX_AGE + Duration::from_secs(5);
        let fresh = g.create_at(later, "run_2", &format!("{PROJECT}/docs")).unwrap();
        assert_eq!((fresh.folder.as_str(), fresh.uses), ("E:/work/project/docs", 0));
    }

    #[test]
    fn only_so_many_runs_hold_a_grant_at_once() {
        let g = RunGrants::new();
        for i in 0..MAX_HELD {
            g.create(&format!("run_{i}"), &src()).unwrap();
        }
        assert!(g.create("one_more", &src()).unwrap_err().contains("too many"));
        assert!(g.release_run("run_0").is_some());
        assert!(g.create("one_more", &src()).is_ok());
    }

    #[test]
    fn a_scope_keeps_each_folder_once_and_names_a_single_one_only_when_there_is_one() {
        assert_eq!(EditScope::new(vec![src()]).single(), Some(src().as_str()));
        assert_eq!(EditScope::new(vec![src(), src()]).single(), Some(src().as_str()), "the same folder twice is one");
        assert_eq!(EditScope::new(vec![src(), format!("{PROJECT}/docs")]).single(), None);
        assert_eq!(EditScope::new(Vec::<String>::new()).single(), None);
        assert_eq!(EditScope::new(vec![String::new(), "  ".into()]).folders.len(), 0);
        assert!(is_edit_tool("fs.edit") && is_edit_tool("fs.write") && is_edit_tool("fs.patch"));
        for tool in ["fs.delete", "fs.move", "fs.copy", "fs.restore", "fs.undo_run", "fs.set_attrs", "term.run", "git.commit", "fs.read", "fs.editx", ""] {
            assert!(!is_edit_tool(tool), "{tool}");
        }
    }
}
