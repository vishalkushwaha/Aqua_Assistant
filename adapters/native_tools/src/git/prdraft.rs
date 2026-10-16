//! The text of a pull request, made from what is on this computer (card OA7-05, TM-17).
//!
//! `git.pr_draft` reads the commits a branch has that its base does not, and the files they change, and writes a title and a description from them. It reads the local repository and
//! nothing else: no forge is asked, nothing is sent, and no model is used (the text is made by rules from commit subjects and file names, never from the lines of a diff). What it writes is
//! a draft for the person to read and change; `forge.pr_create` takes a title and a body of its own and is a different, network tool that asks every time.

/// One commit of the branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub short: String,
    pub subject: String,
}

/// One file the branch changes, with the lines it adds and takes out (`None` for a file git cannot count: a binary one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// The draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub title: String,
    pub body: String,
}

/// The most commits listed in the body, and the most groups of files.
const MAX_COMMITS: usize = 20;
const MAX_GROUPS: usize = 8;
const MAX_NAMED: usize = 6;
const TITLE_CHARS: usize = 72;

/// The numbers of `git diff --numstat`: `12\t3\tsrc/a.rs`, `-\t-\timg.png`. A rename is `{old => new}` or `old => new` and is kept as printed.
pub fn parse_numstat(text: &str) -> Vec<FileChange> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            let (a, r, path) = (it.next()?, it.next()?, it.next()?);
            (!path.is_empty()).then(|| FileChange { path: path.to_string(), added: a.parse().ok(), removed: r.parse().ok() })
        })
        .collect()
}

/// The commits of `git log --pretty=format:%h%x09%s`.
pub fn parse_log(text: &str) -> Vec<Commit> {
    text.lines().filter_map(|l| l.split_once('\t')).map(|(short, subject)| Commit { short: short.trim().to_string(), subject: subject.trim().to_string() }).filter(|c| !c.short.is_empty()).collect()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n.saturating_sub(3)).collect::<String>())
    }
}

/// A branch name a person would say: `feature/login-form` is `Login form`, `fix_42` is `Fix 42`. Empty for a name that says nothing (`HEAD`, a detached commit).
pub fn title_from_branch(branch: &str) -> String {
    let rest = ["feature/", "feat/", "fix/", "bugfix/", "hotfix/", "chore/", "docs/", "refactor/", "aqua/"].iter().find_map(|p| branch.strip_prefix(p)).unwrap_or(branch);
    if rest.is_empty() || rest.eq_ignore_ascii_case("head") || rest.contains("detached") {
        return String::new();
    }
    let words: String = rest.chars().map(|c| if matches!(c, '-' | '_' | '/') { ' ' } else { c }).collect();
    let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Is this a file of tests? A folder or a file named for them.
pub fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase().replace('\\', "/");
    let name = p.rsplit('/').next().unwrap_or("");
    p.split('/').any(|d| matches!(d, "test" | "tests" | "spec" | "specs" | "__tests__" | "e2e"))
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.starts_with("test_")
        || name.rsplit_once('.').is_some_and(|(stem, _)| stem.ends_with("_test") || stem.ends_with("_tests"))
}

/// Is this a document? A Markdown, reStructuredText or text file, or anything under `docs/` or `documentation/`.
pub fn is_doc_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase().replace('\\', "/");
    p.ends_with(".md") || p.ends_with(".rst") || p.ends_with(".adoc") || p.starts_with("docs/") || p.starts_with("documentation/") || p.contains("/docs/")
}

/// The first folder of a path, or the path itself for a file at the top.
fn group_of(path: &str) -> String {
    let p = path.replace('\\', "/");
    // A rename printed as `src/{a => b}/x.rs` is grouped by what is before the brace.
    let p = p.split('{').next().unwrap_or(&p).to_string();
    match p.split_once('/') {
        Some((top, _)) if !top.is_empty() => format!("{top}/"),
        _ => "(top folder)".to_string(),
    }
}

fn names(paths: &[&str]) -> String {
    let shown: Vec<&str> = paths.iter().take(MAX_NAMED).copied().collect();
    let more = paths.len().saturating_sub(MAX_NAMED);
    if more > 0 { format!("{} and {more} more", shown.join(", ")) } else { shown.join(", ") }
}

/// The draft for `commits` (newest first, as git prints them) and `files`, of the branch `branch` (empty when HEAD is on none) that is `ahead` commits past `base`. `uncommitted` is how many files
/// of the working folder are changed and in no commit, when known.
pub fn compose(branch: &str, base: &str, commits: &[Commit], ahead: usize, files: &[FileChange], uncommitted: Option<usize>) -> Draft {
    let oldest_first: Vec<&Commit> = commits.iter().rev().collect();
    let title = match (commits.len(), ahead) {
        (0, _) => String::new(),
        (1, _) => clip(&commits[0].subject, TITLE_CHARS),
        _ => {
            let from_branch = title_from_branch(branch);
            if from_branch.is_empty() {
                // The oldest commit says what the branch began as.
                clip(&format!("{} (and {} more commit{})", oldest_first[0].subject, ahead.max(commits.len()) - 1, if ahead.max(commits.len()) - 1 == 1 { "" } else { "s" }), TITLE_CHARS)
            } else {
                clip(&from_branch, TITLE_CHARS)
            }
        }
    };
    let mut body = String::new();
    body.push_str("## Summary\n\n");
    if commits.is_empty() {
        body.push_str(&format!("This branch has no commit that {base} does not have: there is nothing to propose yet.\n"));
    } else {
        for c in oldest_first.iter().take(MAX_COMMITS) {
            body.push_str(&format!("- {}\n", c.subject));
        }
        if ahead > MAX_COMMITS.min(commits.len()) {
            body.push_str(&format!("- ... and {} more commit{}\n", ahead - MAX_COMMITS.min(commits.len()), if ahead - MAX_COMMITS.min(commits.len()) == 1 { "" } else { "s" }));
        }
    }
    body.push_str("\n## Changes\n\n");
    if files.is_empty() {
        body.push_str("No file differs from the base.\n");
    } else {
        let added: u64 = files.iter().filter_map(|f| f.added).sum();
        let removed: u64 = files.iter().filter_map(|f| f.removed).sum();
        body.push_str(&format!("{} file{} changed, +{added} -{removed}.\n", files.len(), if files.len() == 1 { "" } else { "s" }));
        let mut groups: Vec<(String, usize, u64, u64)> = Vec::new();
        for f in files {
            let g = group_of(&f.path);
            match groups.iter_mut().find(|x| x.0 == g) {
                Some(x) => {
                    x.1 += 1;
                    x.2 += f.added.unwrap_or(0);
                    x.3 += f.removed.unwrap_or(0);
                }
                None => groups.push((g, 1, f.added.unwrap_or(0), f.removed.unwrap_or(0))),
            }
        }
        groups.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (g, n, a, r) in groups.iter().take(MAX_GROUPS) {
            body.push_str(&format!("- {g}: {n} file{}, +{a} -{r}\n", if *n == 1 { "" } else { "s" }));
        }
        if groups.len() > MAX_GROUPS {
            body.push_str(&format!("- ... and {} more folder{}\n", groups.len() - MAX_GROUPS, if groups.len() - MAX_GROUPS == 1 { "" } else { "s" }));
        }
        let tests: Vec<&str> = files.iter().map(|f| f.path.as_str()).filter(|p| is_test_path(p)).collect();
        let docs: Vec<&str> = files.iter().map(|f| f.path.as_str()).filter(|p| is_doc_path(p)).collect();
        body.push('\n');
        if tests.is_empty() {
            body.push_str("Tests: no test file is changed.\n");
        } else {
            body.push_str(&format!("Tests: {} file{} changed ({}).\n", tests.len(), if tests.len() == 1 { "" } else { "s" }, names(&tests)));
        }
        if docs.is_empty() {
            body.push_str("Documentation: no document is changed.\n");
        } else {
            body.push_str(&format!("Documentation: {} file{} changed ({}).\n", docs.len(), if docs.len() == 1 { "" } else { "s" }, names(&docs)));
        }
    }
    body.push_str("\n## Notes\n\n");
    body.push_str(&format!("- {ahead} commit{} ahead of {base}.\n", if ahead == 1 { "" } else { "s" }));
    if let Some(n) = uncommitted.filter(|n| *n > 0) {
        body.push_str(&format!("- Not included: {n} changed file{} of the working folder that {} in no commit.\n", if n == 1 { "" } else { "s" }, if n == 1 { "is" } else { "are" }));
    }
    body.push_str("- Drafted from the local commit log and file list; read it and change it before it is used.\n");
    Draft { title, body }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(s: &str) -> Commit {
        Commit { short: "abc1234".into(), subject: s.into() }
    }

    fn file(p: &str, a: u64, r: u64) -> FileChange {
        FileChange { path: p.into(), added: Some(a), removed: Some(r) }
    }

    #[test]
    fn numstat_and_log_are_read_with_binary_files_and_renames_kept() {
        let f = parse_numstat("12\t3\tsrc/a.rs\n-\t-\timg.png\n1\t0\tsrc/{old => new}/x.rs\n\nbad line\n");
        assert_eq!(f.len(), 3);
        assert_eq!((f[0].added, f[0].removed), (Some(12), Some(3)));
        assert_eq!((f[1].added, f[1].removed), (None, None));
        assert_eq!(f[2].path, "src/{old => new}/x.rs");
        let c = parse_log("1a2b3c4\tfix the parser\n5d6e7f8\tadd a test\n");
        assert_eq!(c, vec![Commit { short: "1a2b3c4".into(), subject: "fix the parser".into() }, Commit { short: "5d6e7f8".into(), subject: "add a test".into() }]);
    }

    #[test]
    fn a_branch_name_becomes_a_title_and_a_name_that_says_nothing_does_not() {
        assert_eq!(title_from_branch("feature/login-form"), "Login form");
        assert_eq!(title_from_branch("fix_42"), "Fix 42");
        assert_eq!(title_from_branch("aqua/wt1"), "Wt1");
        assert_eq!(title_from_branch("HEAD"), "");
        assert_eq!(title_from_branch(""), "");
        assert_eq!(title_from_branch("release/1.2"), "Release 1.2");
    }

    #[test]
    fn test_and_document_files_are_told_by_their_names() {
        for t in ["tests/a.rs", "src/a_test.rs", "src/test_a.py", "web/a.test.ts", "web/a.spec.js", "__tests__/x.js", "adapters/native_tools/tests/git.rs"] {
            assert!(is_test_path(t), "{t}");
        }
        for t in ["src/a.rs", "src/latest.rs", "contest/a.rs", "attestation.md"] {
            assert!(!is_test_path(t), "{t}");
        }
        for d in ["README.md", "docs/a.txt", "documentation/x.png", "a/docs/b.txt", "notes.rst"] {
            assert!(is_doc_path(d), "{d}");
        }
        assert!(!is_doc_path("src/docs.rs") && !is_doc_path("a.rs"));
    }

    #[test]
    fn one_commit_is_its_own_title_and_several_take_the_name_of_the_branch() {
        let one = compose("feature/x", "main", &[commit("Fix the parser")], 1, &[file("src/p.rs", 3, 1)], None);
        assert_eq!(one.title, "Fix the parser");
        let two = compose("feature/login-form", "main", &[commit("newer"), commit("older")], 2, &[file("src/p.rs", 3, 1)], None);
        assert_eq!(two.title, "Login form");
        // A branch that says nothing: the oldest commit and how many more.
        let detached = compose("", "main", &[commit("newest"), commit("middle"), commit("oldest")], 3, &[], None);
        assert_eq!(detached.title, "oldest (and 2 more commits)");
        let none = compose("x", "main", &[], 0, &[], None);
        assert_eq!(none.title, "");
        assert!(none.body.contains("nothing to propose yet") && none.body.contains("No file differs"), "{}", none.body);
        let long = compose("x", "main", &[commit(&"w".repeat(200))], 1, &[], None);
        assert_eq!(long.title.chars().count(), TITLE_CHARS);
    }

    #[test]
    fn the_body_lists_the_commits_oldest_first_the_folders_the_tests_and_the_documents() {
        let d = compose(
            "feature/x",
            "origin/main",
            &[commit("second"), commit("first")],
            2,
            &[file("src/a.rs", 10, 2), file("src/b.rs", 5, 0), file("tests/a.rs", 20, 0), file("docs/guide.md", 4, 1), file("README.md", 1, 1), FileChange { path: "img.png".into(), added: None, removed: None }],
            Some(2),
        );
        let b = &d.body;
        assert!(b.find("- first").unwrap() < b.find("- second").unwrap(), "{b}");
        assert!(b.contains("6 files changed, +40 -4."), "{b}");
        assert!(b.contains("- src/: 2 files, +15 -2"), "{b}");
        assert!(b.contains("Tests: 1 file changed (tests/a.rs)."), "{b}");
        assert!(b.contains("Documentation: 2 files changed (docs/guide.md, README.md)."), "{b}");
        assert!(b.contains("2 commits ahead of origin/main.") && b.contains("Not included: 2 changed files of the working folder that are in no commit."), "{b}");
        assert!(b.contains("Drafted from the local commit log"), "{b}");
        // No test file and no document says so.
        let plain = compose("x", "main", &[commit("c")], 1, &[file("src/a.rs", 1, 0)], Some(0));
        assert!(plain.body.contains("no test file is changed") && plain.body.contains("no document is changed") && !plain.body.contains("Not included"), "{}", plain.body);
    }

    #[test]
    fn a_long_branch_is_cut_and_says_how_many_more() {
        let commits: Vec<Commit> = (0..30).map(|i| commit(&format!("c{i}"))).collect();
        let files: Vec<FileChange> = (0..12).map(|i| file(&format!("d{i}/f.rs"), 1, 0)).collect();
        let d = compose("x", "main", &commits, 35, &files, None);
        assert!(d.body.contains("... and 15 more commits"), "{}", d.body);
        assert!(d.body.contains("... and 4 more folders"), "{}", d.body);
    }
}
