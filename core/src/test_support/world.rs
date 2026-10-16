//! `TempWorld`: a throw-away folder for one test, removed when it is dropped.
//!
//! Three rules, each learned the hard way in this code base:
//!
//! * The folder is named `aqua_<name>_<process id>_<counter>` under the system temp folder, so two tests (and two test runs at the same
//!   time) never share one, and a leftover can be told from anything else.
//! * It is removed **without following a junction or a link**: a world may hold a link to a real folder (the second core's sandbox links
//!   to the model files), and a recursive delete that followed it would delete the real files. A link is removed as a link, the
//!   folder it points to is left alone. A read-only file (git stores its objects read-only) is made writable and removed.
//! * It is never made inside Aqua's own `data` or `logs` folder of this checkout: a test that builds its world there would test, and
//!   then delete, the owner's data.
//!
//! The world sets nothing global. A test that wants Aqua to use it as its data folder says so with [`TempWorld::as_workspace_root`].

use super::env::EnvGuard;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A throw-away folder. Everything below `root` is removed when the value is dropped.
#[derive(Debug)]
pub struct TempWorld {
    pub root: PathBuf,
}

impl TempWorld {
    /// A new, empty world under the system temp folder.
    pub fn new(name: &str) -> Self {
        Self::new_in(&std::env::temp_dir(), name)
    }

    /// A new, empty world under `parent`. Panics when `parent` is inside Aqua's `data` or `logs` folder of this checkout.
    pub fn new_in(parent: &Path, name: &str) -> Self {
        let clean: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        let root = parent.join(format!("aqua_{clean}_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst)));
        if let Some(forbidden) = protected_folder_holding(&root) {
            panic!("TempWorld refuses to make {} inside Aqua's own folder {}: a test world there would be Aqua's real data", root.display(), forbidden.display());
        }
        let _ = remove_tree(&root);
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("TempWorld could not make {}: {e}", root.display()));
        Self { root }
    }

    /// `<root>/<relative>`.
    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// `<root>/data`, made. The folder Aqua keeps its data in when this world is its workspace root.
    pub fn data(&self) -> PathBuf {
        let data = self.root.join("data");
        std::fs::create_dir_all(&data).unwrap_or_else(|e| panic!("TempWorld could not make {}: {e}", data.display()));
        data
    }

    /// Writes a file below the root (the folders in between are made) and returns its path.
    pub fn write(&self, relative: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("TempWorld could not make {}: {e}", dir.display()));
        }
        std::fs::write(&path, contents).unwrap_or_else(|e| panic!("TempWorld could not write {}: {e}", path.display()));
        path
    }

    /// Makes this world the folder Aqua keeps its data in (`AQUA_WORKSPACE_ROOT`) for as long as the returned guard lives. **The whole process sees it**,
    /// so a test that runs beside others in one process (every unit test of this crate) changes what they read from `data/` while the guard lives: prefer
    /// `runtime::data_tripwire::with_workspace_root(&world.root, || ..)` (this thread only). This one is for code that reads the variable itself or runs on
    /// threads of its own, in a test binary where nothing else races it (see `core/tests/test_support_test.rs`).
    pub fn as_workspace_root(&self) -> EnvGuard {
        let root = self.root.to_string_lossy();
        EnvGuard::set(&[(aqua_mcp_adapters::browser_mcp::health::WORKSPACE_ROOT_ENV, root.as_ref())])
    }
}

impl Drop for TempWorld {
    fn drop(&mut self) {
        // A failure to remove must not turn a passing test into a panic inside a drop (a panic while unwinding aborts the process).
        let _ = remove_tree(&self.root);
    }
}

/// Removes a file, a link or a folder with everything in it, and never follows a link: a junction or a symbolic link inside the tree is
/// removed itself and what it points to is untouched. A missing path is not an error.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if is_link(&meta) {
        return remove_link(path);
    }
    if meta.is_dir() {
        // One file that cannot be removed (a database that a part of the program keeps open for the life of the process, which Windows will not let go of) does not stop the others from
        // going: the first failure is returned when everything else has been tried (card OA2-04: a world that was left whole because of one open file was 28 folders for each run).
        let mut first_failure = None;
        for entry in std::fs::read_dir(path)? {
            if let Err(e) = entry.and_then(|entry| remove_tree(&entry.path())) {
                first_failure.get_or_insert(e);
            }
        }
        if let Some(e) = first_failure {
            return Err(e);
        }
        std::fs::remove_dir(path).or_else(|first| {
            make_writable(path);
            std::fs::remove_dir(path).map_err(|_| first)
        })
    } else {
        std::fs::remove_file(path).or_else(|first| {
            make_writable(path);
            std::fs::remove_file(path).map_err(|_| first)
        })
    }
}

/// A symbolic link, or on Windows any reparse point (a junction is one).
fn is_link(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink() || is_reparse_point(meta)
}

#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Removes the link itself: `remove_dir` takes a junction or a link to a folder away without looking inside it, `remove_file` a link to a file.
fn remove_link(path: &Path) -> io::Result<()> {
    std::fs::remove_dir(path).or_else(|_| std::fs::remove_file(path))
}

fn make_writable(path: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        let mut permissions = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

/// The part of the checkout that tests must never build in or delete: its `data` and `logs` folders. The checkout is the one this crate
/// was built from (known at compile time), so the answer does not depend on the environment a test sets.
fn protected_folder_holding(candidate: &Path) -> Option<PathBuf> {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    let candidate = resolved(candidate);
    ["data", "logs"].iter().map(|f| checkout.join(f)).find(|folder| starts_with_ignoring_case(&candidate, &resolved(folder)))
}

/// The path made absolute and clean (`.` and `..` folded away), with the part that exists resolved through links, so that a short name or
/// a link in front of it cannot hide where it really is. The part that does not exist yet is kept as written.
fn resolved(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf()) };
    let mut clean = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other.as_os_str()),
        }
    }
    let mut existing = clean.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent().map(Path::to_path_buf)) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return clean,
        }
    }
}

fn starts_with_ignoring_case(path: &Path, base: &Path) -> bool {
    let lower = |p: &Path| -> Vec<String> { p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect() };
    let (path, base) = (lower(path), lower(base));
    path.len() >= base.len() && path[..base.len()] == base[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_world_is_empty_unique_and_gone_when_dropped() {
        let (a, b) = (TempWorld::new("world_a"), TempWorld::new("world_a"));
        assert_ne!(a.root, b.root, "two worlds of the same name are two folders");
        assert!(a.root.is_dir() && std::fs::read_dir(&a.root).unwrap().next().is_none());
        let file = a.write("deep/er/file.txt", "hello");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
        assert!(a.data().ends_with("data") && a.data().is_dir());
        let root = a.root.clone();
        drop(a);
        assert!(!root.exists(), "the folder and everything in it is removed");
        assert!(b.root.exists(), "the other world is not touched");
    }

    /// Card OA2-04: one file that is held open (a database that a part of the program keeps for the life of the process) stopped the removal of the whole tree at the first failure, so a world
    /// was left whole. The others go, and the failure is still returned.
    #[cfg(windows)]
    #[test]
    fn a_file_that_cannot_be_removed_does_not_keep_the_others_from_going() {
        use std::os::windows::fs::OpenOptionsExt;
        let w = TempWorld::new("world_held");
        for name in ["a.txt", "b_held.txt", "c.txt", "sub/d.txt"] {
            w.write(name, "x");
        }
        // Opened for reading with no sharing: nothing can delete it while the handle is held.
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(w.path("b_held.txt")).expect("the file opens");
        assert!(remove_tree(&w.root).is_err(), "what could not be removed is said");
        assert!(!w.path("a.txt").exists() && !w.path("c.txt").exists() && !w.path("sub").exists(), "the files that could go have gone");
        assert!(w.path("b_held.txt").exists(), "and the held one is still there");
        drop(held);
        assert!(remove_tree(&w.root).is_ok() && !w.root.exists(), "once it is let go the rest is removed");
    }

    #[test]
    fn a_name_with_odd_characters_makes_a_plain_folder_name() {
        let w = TempWorld::new("a b/c\\d:e");
        let name = w.root.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("aqua_a_b_c_d_e_") && !name.contains(' '), "{name}");
        assert_eq!(w.root.parent().unwrap(), std::env::temp_dir());
    }

    #[test]
    fn a_read_only_file_does_not_stop_the_removal() {
        let w = TempWorld::new("world_readonly");
        let file = w.write("objects/ab/cdef", "x");
        let mut p = std::fs::metadata(&file).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(&file, p).unwrap();
        let root = w.root.clone();
        drop(w);
        assert!(!root.exists(), "git keeps its objects read-only and a plain delete leaves them behind on Windows");
    }

    /// Makes `link` a junction (Windows) or a symbolic link (elsewhere) to `target`. False when the machine does not allow it.
    fn link_to(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(link).arg(target).output().map(|o| o.status.success()).unwrap_or(false)
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    #[test]
    fn a_link_inside_the_world_is_removed_as_a_link_and_what_it_points_to_is_left_alone() {
        let outside = TempWorld::new("world_outside");
        let precious = outside.write("precious/keep.txt", "do not delete");
        let w = TempWorld::new("world_link");
        w.write("a.txt", "a");
        if !link_to(&w.path("link"), &outside.path("precious")) {
            eprintln!("skipped: this machine does not let the test make a junction or a link");
            return;
        }
        assert!(w.path("link").join("keep.txt").exists(), "the link works");
        let root = w.root.clone();
        drop(w);
        assert!(!root.exists(), "the world is gone, link and all");
        assert_eq!(std::fs::read_to_string(&precious).unwrap(), "do not delete", "the folder the link pointed to was not followed");
    }

    #[test]
    fn remove_tree_of_a_path_that_is_not_there_is_not_an_error() {
        let w = TempWorld::new("world_missing");
        assert!(remove_tree(&w.path("nothing/here")).is_ok());
    }

    #[test]
    fn a_world_inside_aquas_data_or_logs_folder_is_refused_and_nothing_is_made_there() {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for folder in ["data", "logs"] {
            let parent = checkout.join(folder).join("deeper");
            let before = parent.exists();
            let result = std::panic::catch_unwind(|| TempWorld::new_in(&parent, "refused"));
            let message = *result.err().expect("a world there must be refused").downcast::<String>().expect("the refusal says why");
            assert!(message.contains("refuses") && message.contains("Aqua's own folder"), "{message}");
            assert_eq!(parent.exists(), before, "nothing was made in {folder}");
        }
        // The spelling of the path does not matter: another case, a `..` in front.
        let sneaky = checkout.join("core").join("..").join("DATA");
        assert!(std::panic::catch_unwind(|| TempWorld::new_in(&sneaky, "refused")).is_err());
    }

    #[test]
    fn the_folders_next_to_data_are_not_taken_for_it() {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        assert!(protected_folder_holding(&checkout.join("data_backup").join("x")).is_none());
        assert!(protected_folder_holding(&checkout.join("data").join("x")).is_some());
        assert!(protected_folder_holding(&std::env::temp_dir().join("x")).is_none());
    }
}
