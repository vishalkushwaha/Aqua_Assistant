//! What a run knows about the project it works in (card OA11-01; task EX-06 of the OpenCode audit).
//!
//! When a run starts it is given a [`ProjectContext`]: the folder of its project, where the project's notes file (`AGENTS.md`, ...) stands, and the project's `.aqua` pack with the
//! standing of each of its two kinds (text and code; see [`crate::project_pack`]). The context is **resolved once, at the start of the run, and kept for that run** (not a global): a
//! pack that is edited while the run goes on is the pack of the next run. Every consumer asks the context, and the context answers with a file **only when its kind is trusted**, so a
//! pack that is not trusted is invisible: no skill is listed, no hook runs, no command is offered.
//!
//! Packs are read only when `AQUA_PROJECT_PACK=on`; with it off the context still says which folder the run is in (the hooks may ask to run there), and nothing of any pack is read.

use crate::project_instructions;
use crate::project_pack::{self, Kind, Pack, PackFile, Standing, Standings};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// The standing of a project's pack, and the files as they were read.
#[derive(Debug, Clone)]
pub struct PackStanding {
    pub text: Standing,
    pub code: Standing,
    pack: Pack,
}

impl PackStanding {
    pub fn none() -> Self {
        Self { text: Standing::NoFile, code: Standing::NoFile, pack: Pack::default() }
    }

    /// The files of a kind, **only when that kind is trusted**; none otherwise.
    pub fn files_of(&self, kind: Kind) -> Vec<&PackFile> {
        let trusted = match kind {
            Kind::Text => self.text.is_trusted(),
            Kind::Code => self.code.is_trusted(),
        };
        if trusted {
            self.pack.of_kind(kind)
        } else {
            Vec::new()
        }
    }

    /// What was left out of the pack when it was read, and why (said whatever the standing).
    pub fn warnings(&self) -> &[String] {
        &self.pack.warnings
    }

    /// Every file that was read, whatever the standing: for the page that shows the pack to the person, never for a consumer.
    pub fn pack(&self) -> &Pack {
        &self.pack
    }
}

/// The project a run works in.
#[derive(Debug, Clone)]
pub struct ProjectContext {
    /// The project's folder: the folder that holds `.aqua`, or the folder a goal names when there is none. `None` when no folder is known.
    pub root: Option<PathBuf>,
    /// The notes file (`AQUA.md`, `AGENTS.md`, `CLAUDE.md`) and whether the person trusts it.
    pub instructions: project_instructions::Standing,
    /// The `.aqua` pack.
    pub pack: PackStanding,
    /// Why a pack was not looked at or not found (a link, a file called `.aqua`).
    pub warnings: Vec<String>,
}

impl ProjectContext {
    /// A context for a run with no project.
    pub fn none() -> Self {
        Self { root: None, instructions: project_instructions::Standing::NoFile, pack: PackStanding::none(), warnings: Vec::new() }
    }

    /// Looks at the project of `start` (a folder inside `limit`, the root of the folders Aqua works in): the notes file and the pack, at most four levels up and never above `limit`.
    /// `key` is the trust key (`None`: nothing is trusted). Reads the files of the pack once.
    pub fn resolve(data_dir: &Path, key: Option<&[u8]>, start: &Path, limit: &Path) -> ProjectContext {
        let instructions = project_instructions::standing(data_dir, start, limit);
        let found = project_pack::find(start, limit);
        match found.root {
            Some(root) => {
                let pack = project_pack::scan(&root.join(project_pack::DIR));
                let Standings { text, code } = project_pack::standings(data_dir, key, &root, &pack);
                ProjectContext { root: Some(root), instructions, pack: PackStanding { text, code, pack }, warnings: found.warnings }
            }
            None => ProjectContext { root: Some(start.to_path_buf()), instructions, pack: PackStanding::none(), warnings: found.warnings },
        }
    }

    /// The text files of the pack as (path inside `.aqua`, bytes): skills, commands and agents, **only when the text kind is trusted**.
    pub fn text_files(&self) -> Vec<(String, Vec<u8>)> {
        self.pack.files_of(Kind::Text).into_iter().map(|f| (f.rel.clone(), f.bytes.clone())).collect()
    }

    /// One code file of the pack (`hooks.json`, `mcp.json`, `format.json`, `tools.json`), **only when the code kind is trusted**.
    pub fn code_file(&self, name: &str) -> Option<Vec<u8>> {
        self.pack.files_of(Kind::Code).into_iter().find(|f| f.rel == name).map(|f| f.bytes.clone())
    }
}

/// Are packs read at all (`AQUA_PROJECT_PACK=on`)?
pub fn enabled() -> bool {
    crate::flags::is_on(&crate::flags::PROJECT_PACK)
}

struct RunEntry {
    /// The folder the run's goal names (or the first root).
    project: PathBuf,
    /// Set when packs are on and the project has a pack.
    context: Option<Arc<ProjectContext>>,
}

static RUNS: Mutex<Option<HashMap<String, RunEntry>>> = Mutex::new(None);

fn with_runs<R>(f: impl FnOnce(&mut HashMap<String, RunEntry>) -> R) -> R {
    let mut g = RUNS.lock().unwrap_or_else(PoisonError::into_inner);
    f(g.get_or_insert_with(HashMap::new))
}

/// A run starts: the folder of its project is found from its goal (inside `roots`), and when packs are on the context of the project is resolved and kept for the run. `key` is asked for
/// only when packs are on and a pack is there. Returns the context when there is one to use.
pub fn begin_run(run_id: &str, goal: &str, roots: &[PathBuf], data_dir: &Path, key: &dyn Fn() -> Option<Vec<u8>>) -> Option<Arc<ProjectContext>> {
    if run_id.is_empty() {
        return None;
    }
    let Some(start) = project_instructions::project_dir_for_goal(goal, roots) else { return None };
    let real_roots: Vec<PathBuf> = roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    let context = if enabled() {
        real_roots.iter().find(|r| aqua_native_tools::fs::guard::within(&start, r)).and_then(|limit| {
            let found = project_pack::find(&start, limit);
            found.root.as_ref()?;
            Some(Arc::new(ProjectContext::resolve(data_dir, key().as_deref(), &start, limit)))
        })
    } else {
        None
    };
    let project = context.as_ref().and_then(|c| c.root.clone()).unwrap_or(start);
    with_runs(|runs| {
        // A table that only grows would be a leak: runs end through `end_run`, but a run that never ends cleanly must not keep it growing for ever.
        if runs.len() >= 256 {
            runs.clear();
        }
        runs.insert(run_id.to_string(), RunEntry { project, context: context.clone() });
    });
    context
}

/// A helper that a run started (card OA11-04) works in the project of that run, with the same context: the pack the person trusted for the run is the pack of the helper (its agents, its
/// skills), and nothing is resolved again.
pub fn inherit(parent_run: &str, child_run: &str) {
    if parent_run.is_empty() || child_run.is_empty() {
        return;
    }
    with_runs(|runs| {
        if let Some(entry) = runs.get(parent_run) {
            let copy = RunEntry { project: entry.project.clone(), context: entry.context.clone() };
            runs.insert(child_run.to_string(), copy);
        }
    });
}

/// The run is over: its context is let go.
pub fn end_run(run_id: &str) {
    with_runs(|runs| {
        runs.remove(run_id);
    });
}

/// The context of a run, when it has one (packs on, and the project has a pack).
pub fn for_run(run_id: &str) -> Option<Arc<ProjectContext>> {
    with_runs(|runs| runs.get(run_id).and_then(|e| e.context.clone()))
}

/// The folder of the project a run works in.
pub fn project_dir_of_run(run_id: &str) -> Option<PathBuf> {
    with_runs(|runs| runs.get(run_id).map(|e| e.project.clone()))
}

/// The text files of the pack of a run (skills, commands, agents), only when the person trusts the text kind. Empty for a run with no pack.
pub fn pack_text_files(run_id: &str) -> Vec<(String, Vec<u8>)> {
    for_run(run_id).map(|c| c.text_files()).unwrap_or_default()
}

/// One code file of the pack of a run, only when the person trusts the code kind.
pub fn pack_code_file(run_id: &str, name: &str) -> Option<Vec<u8>> {
    for_run(run_id).and_then(|c| c.code_file(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = &[3u8; 32];

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_pctx_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("data")).unwrap();
        std::fs::create_dir_all(d.join("ws/proj/.aqua/skills/deploy")).unwrap();
        std::fs::create_dir_all(d.join("ws/proj/src")).unwrap();
        std::fs::write(d.join("ws/proj/.aqua/skills/deploy/SKILL.md"), "---\nname: deploy\ndescription: Deploy it\n---\nsteps").unwrap();
        std::fs::write(d.join("ws/proj/.aqua/hooks.json"), r#"{"hooks": []}"#).unwrap();
        d
    }

    #[test]
    fn an_untrusted_pack_is_invisible_to_every_consumer() {
        let d = temp("invisible");
        let ctx = ProjectContext::resolve(&d.join("data"), Some(KEY), &d.join("ws/proj/src"), &d.join("ws"));
        assert_eq!(std::fs::canonicalize(ctx.root.as_ref().unwrap()).unwrap(), std::fs::canonicalize(d.join("ws/proj")).unwrap());
        assert_eq!((ctx.pack.text.clone(), ctx.pack.code.clone()), (Standing::NotTrusted, Standing::NotTrusted));
        assert!(ctx.text_files().is_empty() && ctx.code_file("hooks.json").is_none(), "nothing is handed out before the person trusts it");
        assert!(ctx.pack.files_of(Kind::Text).is_empty() && ctx.pack.files_of(Kind::Code).is_empty());
        assert_eq!(ctx.pack.pack().files.len(), 2, "the files were read (for the page that shows them), but no consumer is given one");
    }

    #[test]
    fn trusting_text_hands_out_the_text_files_and_not_the_code() {
        let d = temp("text_only");
        let root = d.join("ws/proj");
        let pack = project_pack::scan(&root.join(".aqua"));
        project_pack::trust(&d.join("data"), KEY, &root, &pack, &[Kind::Text], &Default::default()).unwrap();
        let ctx = ProjectContext::resolve(&d.join("data"), Some(KEY), &root, &d.join("ws"));
        assert_eq!(ctx.text_files().len(), 1);
        assert_eq!(ctx.text_files()[0].0, "skills/deploy/SKILL.md");
        assert!(ctx.code_file("hooks.json").is_none(), "the code is a separate yes");
        project_pack::trust(&d.join("data"), KEY, &root, &pack, &[Kind::Code], &Default::default()).unwrap();
        let ctx = ProjectContext::resolve(&d.join("data"), Some(KEY), &root, &d.join("ws"));
        assert_eq!(ctx.code_file("hooks.json").as_deref(), Some(br#"{"hooks": []}"#.as_slice()));
        // A byte changed after the trust: the next run's context hands out nothing of that kind.
        std::fs::write(root.join(".aqua/hooks.json"), r#"{"hooks": [ ]}"#).unwrap();
        let ctx = ProjectContext::resolve(&d.join("data"), Some(KEY), &root, &d.join("ws"));
        assert!(ctx.code_file("hooks.json").is_none() && matches!(ctx.pack.code, Standing::Changed { .. }));
        assert_eq!(ctx.text_files().len(), 1, "the text is as it was trusted");
        // No key: nothing at all.
        let ctx = ProjectContext::resolve(&d.join("data"), None, &root, &d.join("ws"));
        assert!(ctx.text_files().is_empty());
    }

    /// Card OA11-04: a helper that a run starts works in the project of that run, with the same context (the trusted pack is the helper's too), and a run that has none gives it none.
    #[test]
    fn a_helper_works_in_the_project_of_the_run_that_started_it_and_gets_nothing_from_a_run_that_has_none() {
        let d = temp("inherit");
        let root = d.join("ws/proj");
        let pack = project_pack::scan(&root.join(".aqua"));
        project_pack::trust(&d.join("data"), KEY, &root, &pack, &[Kind::Text], &Default::default()).unwrap();
        let ctx = Arc::new(ProjectContext::resolve(&d.join("data"), Some(KEY), &root, &d.join("ws")));
        with_runs(|runs| {
            runs.insert("run_inherit_parent".into(), RunEntry { project: root.clone(), context: Some(ctx.clone()) });
        });
        inherit("run_inherit_parent", "run_inherit_child");
        assert_eq!(project_dir_of_run("run_inherit_child"), Some(root.clone()));
        assert_eq!(pack_text_files("run_inherit_child").len(), 1, "the pack the person trusted for the run is the helper's");
        inherit("", "run_inherit_a");
        inherit("run_inherit_nobody", "run_inherit_b");
        assert!(project_dir_of_run("run_inherit_a").is_none() && project_dir_of_run("run_inherit_b").is_none());
        end_run("run_inherit_parent");
        assert!(for_run("run_inherit_child").is_some(), "the helper's own entry is its own: it goes when it ends");
        end_run("run_inherit_child");
        assert!(for_run("run_inherit_child").is_none());
    }

    #[test]
    fn a_folder_with_no_pack_has_the_folder_and_no_pack() {
        let d = temp("nopack");
        std::fs::create_dir_all(d.join("ws/plain/src")).unwrap();
        let ctx = ProjectContext::resolve(&d.join("data"), Some(KEY), &d.join("ws/plain/src"), &d.join("ws"));
        assert_eq!((ctx.pack.text.clone(), ctx.pack.code.clone()), (Standing::NoFile, Standing::NoFile));
        assert_eq!(ctx.root, Some(d.join("ws/plain/src")));
        assert_eq!(ProjectContext::none().root, None);
    }
}
