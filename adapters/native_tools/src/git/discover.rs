//! Finding the git repositories inside the folders Aqua may use (card OA7-02, TM-05 and CU-07).
//!
//! Without this the git tools needed a hand-written `data/tools/repos.json`, and on a fresh install every `git.*` call answered "no repositories are configured". The file tools already
//! know the folders the person allowed (the roots of `fs.json`, the project folder under full access, a folder allowed in a request): the repositories that sit in those folders are
//! found here, by looking for a `.git`, and are offered to the git tools as read-only-by-default entries (not trusted: every write asks, as it does for any repository the person did not name).
//!
//! What is looked at is a few folders and nothing inside them. A folder is a repository when it has a `.git` that is a real folder (with a `HEAD` file, and a `config` that is not a link), or
//! a `.git` file that says `gitdir: <folder>` where that folder exists and is itself inside the folders Aqua may use (a linked worktree or a submodule whose real repository is somewhere
//! else is left alone). A link (a symbolic link or a junction) is never followed. The repository's own configuration is not read here: what it may make git run is switched off, by git,
//! on every call (`Inner::programs_switched_off`).

use crate::fs::guard::{canonical, Op, PathGuard};
use std::path::{Path, PathBuf};

/// How many folders below a root are looked into: the root itself, its folders, and the folders of those.
pub const MAX_DEPTH: usize = 2;
/// The most folders one search looks at, so that a root that is a whole drive does not make a call slow.
pub const MAX_DIRS: usize = 4000;
/// The most repositories one search lists.
pub const MAX_REPOS: usize = 64;

/// Names of folders that are never looked into: they hold other people's code or build output, not the person's repositories.
const SKIP: &[&str] = &["node_modules", "target", "dist", "build", "venv", "__pycache__", "site-packages"];

/// Is `dir` the top of a git repository that the git tools may use? (See the module text for what that means.) `dir` is a folder the guard has already put inside a root.
pub fn is_repo(guard: &PathGuard, dir: &Path) -> bool {
    let dot_git = dir.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&dot_git) else { return false };
    if meta.file_type().is_symlink() {
        return false;
    }
    if meta.is_dir() {
        // A real repository has a plain `HEAD` file. Its `config` may not be a link: the file tools refuse to name it, and a link would let it be any file.
        let head_ok = std::fs::symlink_metadata(dot_git.join("HEAD")).map(|m| m.is_file()).unwrap_or(false);
        let config_is_link = std::fs::symlink_metadata(dot_git.join("config")).map(|m| m.file_type().is_symlink()).unwrap_or(false);
        return head_ok && !config_is_link;
    }
    if meta.is_file() {
        return gitdir_of_file(&dot_git).is_some_and(|target| {
            let target = if target.is_absolute() { target } else { dir.join(target) };
            // The real repository must be a folder with a `HEAD` and be inside the folders Aqua may use, whatever the file says.
            canonical(&target).ok().is_some_and(|c| c.join("HEAD").is_file() && guard.resolve(&c.to_string_lossy(), Op::Read).is_ok())
        });
    }
    false
}

/// What the `.git` file of a linked worktree or a submodule says: `gitdir: <path>`. Only the first line, only the start of the file.
fn gitdir_of_file(dot_git: &Path) -> Option<PathBuf> {
    use std::io::Read;
    let mut buf = [0u8; 1024];
    let n = std::fs::File::open(dot_git).ok()?.read(&mut buf).ok()?;
    let text = std::str::from_utf8(&buf[..n]).ok()?;
    let rest = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    (!rest.is_empty()).then(|| PathBuf::from(rest))
}

/// A folder that is not a link and is not one of the names that are never looked into.
fn is_folder_to_look_into(parent: &Path, name: &std::ffi::OsStr) -> Option<PathBuf> {
    let name_text = name.to_string_lossy();
    if name_text.starts_with('.') || SKIP.iter().any(|s| name_text.eq_ignore_ascii_case(s)) {
        return None;
    }
    let path = parent.join(name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    (meta.is_dir() && !meta.file_type().is_symlink()).then_some(path)
}

/// The tops of the repositories found in `roots` (each a folder the guard allows), sorted by path and without repeats. A folder that is a repository is not looked into further: the
/// repositories inside it (a submodule, a vendored clone) are its own business.
pub fn find_repos(guard: &PathGuard, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut looked = 0usize;
    for root in roots {
        let mut level: Vec<PathBuf> = vec![root.clone()];
        for depth in 0..=MAX_DEPTH {
            let mut next: Vec<PathBuf> = Vec::new();
            for dir in level {
                looked += 1;
                if looked > MAX_DIRS || found.len() >= MAX_REPOS {
                    break;
                }
                // The guard's own answer for the folder is what the git tools will use: inside a root, not Aqua's own, not a place that holds credentials.
                let Ok(resolved) = guard.resolve(&dir.to_string_lossy(), Op::Read) else { continue };
                if is_repo(guard, &resolved.path) {
                    if !found.contains(&resolved.path) {
                        found.push(resolved.path);
                    }
                    continue;
                }
                if depth == MAX_DEPTH {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                next.extend(entries.flatten().filter_map(|e| is_folder_to_look_into(&dir, &e.file_name())));
            }
            level = next;
        }
    }
    found.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
    found
}

/// A name the git tools take as an alias (1-40 letters, digits, `_` or `-`) made from the name of a folder. A name with nothing usable in it (all letters of another script, say) is `repo`.
pub fn sanitize_alias(name: &str) -> String {
    let mapped: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if !mapped.chars().any(|c| c.is_ascii_alphanumeric()) {
        return "repo".to_string();
    }
    mapped.chars().take(40).collect()
}

/// `base`, or `base-2`, `base-3`... the first that `taken` does not say is in use (the case of a name does not matter), still within 40 characters.
pub fn unique_alias(base: &str, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    for n in 2..1000 {
        let suffix = format!("-{n}");
        let head: String = base.chars().take(40 - suffix.len()).collect();
        let candidate = format!("{head}{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FsConfig;
    use std::sync::Arc;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua_discover_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A folder that looks like a repository to `is_repo` (a `.git` folder with a `HEAD`), without running git.
    fn fake_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    fn guard_of(root: &Path) -> Arc<PathGuard> {
        Arc::new(PathGuard::new(FsConfig::with_workspace(root.to_string_lossy().to_string()), vec![]).unwrap())
    }

    #[test]
    fn aliases_are_made_from_folder_names_and_never_repeat() {
        assert_eq!(sanitize_alias("aqua"), "aqua");
        assert_eq!(sanitize_alias("my project (copy)"), "my_project__copy_");
        assert_eq!(sanitize_alias("परियोजना"), "repo", "nothing usable in the name");
        assert_eq!(sanitize_alias(&"x".repeat(60)).len(), 40);
        let taken = |a: &str| ["aqua", "aqua-2", "AQUA-3"].iter().any(|t| t.eq_ignore_ascii_case(a));
        assert_eq!(unique_alias("aqua", &taken), "aqua-4");
        assert_eq!(unique_alias("other", &taken), "other");
        let long = "y".repeat(40);
        let again = unique_alias(&long, &|a: &str| a == long);
        assert!(again.len() <= 40 && again.ends_with("-2"), "{again}");
    }

    #[test]
    fn repositories_are_found_at_the_root_its_folders_and_theirs_and_not_deeper() {
        let dir = temp("depth");
        let guard = guard_of(&dir);
        let root = canonical(&dir).unwrap();
        fake_repo(&root.join("a"));
        fake_repo(&root.join("group/b"));
        fake_repo(&root.join("group/inner/c"));
        fake_repo(&root.join("g1/g2/too_deep"));
        fake_repo(&root.join("node_modules/pkg"));
        fake_repo(&root.join(".hidden/h"));
        // A folder that is a repository is not looked into: what is inside it is its own.
        fake_repo(&root.join("outer"));
        fake_repo(&root.join("outer/vendored"));
        let found: Vec<String> = find_repos(&guard, &[root.clone()]).iter().map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/")).collect();
        // The root, a folder of it, and a folder of that one: `group/b` is two below the root and is found, `group/inner/c` and `g1/g2/too_deep` are three and are not looked at.
        assert_eq!(found, vec!["a", "group/b", "outer"], "{found:?}");
        // The root itself being a repository is the one found.
        fake_repo(&root);
        assert_eq!(find_repos(&guard, &[root.clone()]), vec![root.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dot_git_that_is_a_link_or_has_a_linked_config_or_no_head_is_not_a_repository() {
        let dir = temp("odd");
        let guard = guard_of(&dir);
        let root = canonical(&dir).unwrap();
        std::fs::create_dir_all(root.join("no_head/.git")).unwrap();
        assert!(!is_repo(&guard, &root.join("no_head")), "a .git folder without a HEAD");
        assert!(!is_repo(&guard, &root.join("missing")));
        fake_repo(&root.join("ok"));
        assert!(is_repo(&guard, &root.join("ok")));
        // A `.git` that is a junction to a real repository is not followed (a junction needs no right on Windows).
        fake_repo(&root.join("real"));
        std::fs::create_dir_all(root.join("via_link")).unwrap();
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(root.join("via_link/.git")).arg(root.join("real/.git")).output().map(|o| o.status.success()).unwrap_or(false);
        if made {
            assert!(!is_repo(&guard, &root.join("via_link")), "a .git that is a junction is a link");
            let _ = std::process::Command::new("cmd").args(["/c", "rmdir"]).arg(root.join("via_link/.git")).output();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_git_file_counts_only_when_its_gitdir_is_a_repository_folder_inside_the_roots() {
        let dir = temp("gitfile");
        let root = canonical(&dir).unwrap();
        let guard = guard_of(&root);
        // The real repository inside the roots, and a linked worktree beside it whose `.git` file names it.
        fake_repo(&root.join("main"));
        std::fs::create_dir_all(root.join("main/.git/worktrees/wt")).unwrap();
        std::fs::write(root.join("main/.git/worktrees/wt/HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::create_dir_all(root.join("wt")).unwrap();
        std::fs::write(root.join("wt/.git"), format!("gitdir: {}\n", root.join("main/.git/worktrees/wt").display())).unwrap();
        assert!(is_repo(&guard, &root.join("wt")), "a worktree whose repository is inside the roots");
        // A relative `gitdir` is read from the folder of the file.
        std::fs::create_dir_all(root.join("rel")).unwrap();
        std::fs::write(root.join("rel/.git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        assert!(is_repo(&guard, &root.join("rel")));
        // One whose `gitdir` is outside the roots, or is not there, or is not a git folder, is not.
        let outside = temp("gitfile_outside");
        fake_repo(&outside.join("real"));
        std::fs::create_dir_all(root.join("away")).unwrap();
        std::fs::write(root.join("away/.git"), format!("gitdir: {}\n", outside.join("real/.git").display())).unwrap();
        assert!(!is_repo(&guard, &root.join("away")), "a gitdir outside the folders Aqua may use");
        std::fs::create_dir_all(root.join("gone")).unwrap();
        std::fs::write(root.join("gone/.git"), "gitdir: ../nowhere\n").unwrap();
        assert!(!is_repo(&guard, &root.join("gone")));
        std::fs::create_dir_all(root.join("junk")).unwrap();
        std::fs::write(root.join("junk/.git"), "not a gitdir line\n").unwrap();
        assert!(!is_repo(&guard, &root.join("junk")));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
