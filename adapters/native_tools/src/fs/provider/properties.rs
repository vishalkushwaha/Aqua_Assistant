//! Property test of the all-or-nothing rule of `fs.patch` (card OA13-01, task QA-06, the first property of its list). A patch that changes several files writes them one after the other,
//! and the promise is that a patch that stops on the way leaves every file as it was, and one that goes through leaves every file as the diff says. To see it, a write is made to fail:
//! `write_atomic_expecting` asks `injected_failure` after the new content is on disk and before the rename, so the failure is the one a full disk or a locked file gives, the temporary
//! file must be cleaned up, and what the patch had done before has to be undone. In a build that is not a test nothing of this exists.
//!
//! A module of the provider (it needs the private `patch`, `restore_group` and the write itself) with the case loop of `tests/prop_kit/mod.rs` of `aqua-tool-host`.

#[path = "../../../../tool_host/tests/prop_kit/mod.rs"]
mod prop_kit;

use super::*;
use prop_kit::{check, Rng};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

thread_local! {
    /// The writes (counted from 0 since the injection was set) that fail.
    static FAILING: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static WRITES: Cell<u32> = const { Cell::new(0) };
}

/// Asked by every write of this thread: counts it, and fails it when it is one of those the test named.
pub(super) fn injected_failure() -> Option<std::io::Error> {
    let n = WRITES.with(|w| {
        let n = w.get();
        w.set(n + 1);
        n
    });
    FAILING.with(|f| f.borrow().contains(&n)).then(|| std::io::Error::other(format!("injected failure of write {n}")))
}

/// The writes of this thread fail at the given places (counted from 0) until it is dropped.
struct Failing;

impl Failing {
    fn at(places: &[u32]) -> Failing {
        WRITES.with(|w| w.set(0));
        FAILING.with(|f| *f.borrow_mut() = places.to_vec());
        Failing
    }

    /// How many writes were made since this was set.
    fn writes(&self) -> u32 {
        WRITES.with(|w| w.get())
    }
}

impl Drop for Failing {
    fn drop(&mut self) {
        FAILING.with(|f| f.borrow_mut().clear());
    }
}

#[derive(Clone, Debug)]
enum Change {
    /// Line `at` of the file becomes this text.
    Update { name: String, at: usize, new: String },
    Delete { name: String },
    Create { name: String, lines: Vec<String> },
}

/// A set of files and a patch over them.
struct Case {
    files: BTreeMap<String, Vec<String>>,
    changes: Vec<Change>,
}

impl Case {
    fn diff(&self) -> String {
        let mut out = String::new();
        for c in &self.changes {
            match c {
                Change::Update { name, at, new } => {
                    let lines = &self.files[name];
                    let from = at.saturating_sub(1);
                    let to = (at + 2).min(lines.len());
                    out.push_str(&format!("--- a/@ws/{name}\n+++ b/@ws/{name}\n@@ -{},{} +{},{} @@\n", from + 1, to - from, from + 1, to - from));
                    for (i, l) in lines.iter().enumerate().take(to).skip(from) {
                        if i == *at {
                            out.push_str(&format!("-{l}\n+{new}\n"));
                        } else {
                            out.push_str(&format!(" {l}\n"));
                        }
                    }
                }
                Change::Delete { name } => {
                    let lines = &self.files[name];
                    out.push_str(&format!("--- a/@ws/{name}\n+++ /dev/null\n@@ -1,{} +0,0 @@\n", lines.len()));
                    for l in lines {
                        out.push_str(&format!("-{l}\n"));
                    }
                }
                Change::Create { name, lines } => {
                    out.push_str(&format!("--- /dev/null\n+++ b/@ws/{name}\n@@ -0,0 +1,{} @@\n", lines.len()));
                    for l in lines {
                        out.push_str(&format!("+{l}\n"));
                    }
                }
            }
        }
        out
    }

    /// The files a patch that goes through leaves, by name.
    fn after(&self) -> BTreeMap<String, String> {
        let mut files: BTreeMap<String, String> = self.files.iter().map(|(n, l)| (n.clone(), text_of(l))).collect();
        for c in &self.changes {
            match c {
                Change::Update { name, at, new } => {
                    let mut lines = self.files[name].clone();
                    lines[*at] = new.clone();
                    files.insert(name.clone(), text_of(&lines));
                }
                Change::Delete { name } => {
                    files.remove(name);
                }
                Change::Create { name, lines } => {
                    files.insert(name.clone(), text_of(lines));
                }
            }
        }
        files
    }

    fn before(&self) -> BTreeMap<String, String> {
        self.files.iter().map(|(n, l)| (n.clone(), text_of(l))).collect()
    }
}

fn text_of(lines: &[String]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn make_case(rng: &mut Rng) -> Case {
    let n = rng.range(2, 5);
    let mut files = BTreeMap::new();
    for i in 0..n {
        let name = if rng.chance(30) { format!("sub/f{i}.txt") } else { format!("f{i}.txt") };
        let lines: Vec<String> = (0..rng.range(3, 6)).map(|k| format!("file {i} line {k} {}", rng.word(2, 6))).collect();
        files.insert(name, lines);
    }
    let mut changes = Vec::new();
    for (name, lines) in &files {
        match rng.below(10) {
            0..=5 => changes.push(Change::Update { name: name.clone(), at: rng.below(lines.len()), new: format!("CHANGED {}", rng.word(3, 8)) }),
            6 => changes.push(Change::Delete { name: name.clone() }),
            _ => {}
        }
    }
    for k in 0..rng.range(0, 2) {
        let name = if rng.chance(40) { format!("made/new{k}.txt") } else { format!("new{k}.txt") };
        changes.push(Change::Create { name, lines: (0..rng.range(1, 3)).map(|i| format!("created {k} line {i} {}", rng.word(2, 5))).collect() });
    }
    // A patch has to write at least once for there to be a write to fail.
    if !changes.iter().any(|c| !matches!(c, Change::Delete { .. })) {
        let (name, lines) = files.iter().next().map(|(n, l)| (n.clone(), l.clone())).unwrap();
        changes.retain(|c| !matches!(c, Change::Delete { name: d } if *d == name));
        changes.push(Change::Update { name, at: 0, new: format!("CHANGED {} {}", lines.len(), rng.word(3, 8)) });
    }
    rng.shuffle(&mut changes);
    Case { files, changes }
}

struct World {
    dir: PathBuf,
    guard: PathGuard,
    trash: Trash,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn world(case: &Case, tag: &str) -> World {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("aqua_fsprop_{tag}_{}_{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    for (name, lines) in &case.files {
        let path = dir.join("ws").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text_of(lines)).unwrap();
    }
    let guard = PathGuard::new(super::super::guard::FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string()), vec![dir.join("aqua")]).unwrap();
    let trash = Trash::new(dir.join("aqua").join("trash")).unwrap();
    World { dir, guard, trash }
}

/// Every file under the workspace, by its name there, and a check that no temporary file of a write is left.
fn files_of(w: &World) -> BTreeMap<String, String> {
    fn walk(base: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(at).unwrap().flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let name = path.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
                assert!(!name.contains(".aqua-tmp-"), "a temporary file of a write was left behind: {name}");
                out.insert(name, std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&w.dir.join("ws"), &w.dir.join("ws"), &mut out);
    out
}

#[test]
fn a_patch_that_fails_at_any_write_leaves_every_file_as_it_was_and_one_that_goes_through_leaves_them_as_the_diff_says() {
    check("patch_fails_at_the_kth_write", 12, |rng, _| {
        let case = make_case(rng);
        let args = json!({"diff": case.diff()});
        // How many writes a patch that goes through makes (the creations and the updates), and that it leaves what the diff says.
        let writes = {
            let w = world(&case, "count");
            let failing = Failing::at(&[]);
            let done = patch(&w.guard, &w.trash, &args);
            assert!(done.is_ok(), "a patch made from a change does not apply: {:?}: diff {}", done.err().map(|e| e.message), case.diff());
            assert_eq!(files_of(&w), case.after(), "the files after a patch that went through: diff {}", case.diff());
            failing.writes()
        };
        assert!(writes >= 1, "no write: {:?}", case.changes);
        // A failure at each of the writes in turn.
        // One folder for all of them: a patch that fails leaves every file as it was, which is what is asserted after each, so the next failure starts from the same files.
        let w = world(&case, "fail");
        for k in 0..writes {
            let _failing = Failing::at(&[k]);
            let said = patch(&w.guard, &w.trash, &args).err().unwrap_or_else(|| panic!("write {k} of {writes} failed and the patch said it went through: diff {}", case.diff()));
            assert!(said.message.contains("put back") && !said.message.contains("except"), "write {k} of {writes}: the files already written were not all put back: {}", said.message);
            assert_eq!(files_of(&w), case.before(), "write {k} of {writes} failed and some file is not as it was: diff {}", case.diff());
        }
    });
}

#[test]
fn when_the_undoing_fails_too_the_message_says_which_file_and_the_undo_group_puts_everything_back() {
    check("patch_fails_twice", 8, |rng, _| {
        let case = make_case(rng);
        let args = json!({"diff": case.diff()});
        let writes = {
            let w = world(&case, "count2");
            let failing = Failing::at(&[]);
            patch(&w.guard, &w.trash, &args).expect("the patch goes through");
            failing.writes()
        };
        let w = world(&case, "fail2");
        for k in 1..writes {
            // The write k fails, and the first write that undoes an earlier one fails as well.
            let _failing = Failing::at(&[k, k + 1]);
            let said = patch(&w.guard, &w.trash, &args).err().unwrap_or_else(|| panic!("write {k} failed and the patch said it went through: diff {}", case.diff()));
            let stuck = said.message.contains("except");
            let group = said.message.split("undo_id=").nth(1).and_then(|r| r.split_whitespace().next()).map(str::to_string);
            if stuck {
                let group = group.unwrap_or_else(|| panic!("a file is stuck and the message names no undo group: {}", said.message));
                restore_group(&w.guard, &w.trash, &group, true).unwrap_or_else(|e| panic!("the undo group does not put the files back: {}: {}", e.message, said.message));
            }
            assert_eq!(files_of(&w), case.before(), "write {k} failed (and the undoing of one write), stuck={stuck}, and after the undo group some file is not as it was: {} diff {}", said.message, case.diff());
        }
    });
}
