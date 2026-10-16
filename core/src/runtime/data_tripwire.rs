//! The data-folder tripwire for tests (task QA-04 of the OpenCode audit; CX-15).
//!
//! `runtime::data_dir()` is `<workspace root>/data`, and in a checkout the workspace root is the repository: a unit test whose code path
//! asks for it without giving itself a folder of its own reads and writes the owner's real tokens, databases and tracked files (a loop
//! test used to rewrite `data/cache/agent_prompt_start.txt` this way). The tripwire makes such a test visible, and can make it fail.
//!
//! * **Report mode** (the default): the first time a test thread looks up the real folder, one line goes to the real standard error
//!   (not the captured one, so it shows without `--nocapture`): `REAL DATA FOLDER REACHED by <test> at <file>:<line>`. The tests are not
//!   failed. `offenders()` lists them all.
//! * **Strict mode** (`AQUA_TEST_STRICT_DATA=1`): the lookup panics with a plain sentence naming the test, so the test fails.
//!
//! "The real folder" is the one `aqua_mcp_adapters::natural_workspace_root()` gives (the checkout, or `%LOCALAPPDATA%\Aqua` for an
//! installed copy), whether it comes from the lookup rule or from `AQUA_WORKSPACE_ROOT` set to it. A test that has its own folder (a
//! [`TempWorld`](crate::test_support::TempWorld) given to its thread with [`with_workspace_root`] / [`with_data_dir`], or made the workspace root
//! of the whole process with `as_workspace_root`, which every test beside it then reads from too) is never reported. A test that means to look at the real folder says so with [`allow_real_data`].
//!
//! The lookup is heard through the observer of `aqua_mcp_adapters::browser_mcp::health` (installed by the first call of
//! `runtime::workspace_root()` in a test binary), so a direct call of `aqua_workspace_root()` is heard too once that has happened.
//! Compiled for the crate's own tests and with the `test-support` feature; a release build has none of it.

use std::cell::{Cell, RefCell};
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

/// Set to anything but `0` or empty to fail the test that reaches the real data folder.
pub const STRICT_ENV: &str = "AQUA_TEST_STRICT_DATA";

static OFFENDERS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static INSTALLED: Once = Once::new();

thread_local! {
    /// How many [`allow_real_data`] calls this thread is inside.
    static ALLOWED: Cell<usize> = const { Cell::new(0) };
    /// The folders a test gave its own thread: the workspace root and the data folder.
    static OVERRIDE: RefCell<(Option<PathBuf>, Option<PathBuf>)> = const { RefCell::new((None, None)) };
    /// Strictness for this thread only, when a test needs to decide it (the tests of the tripwire itself).
    static STRICT_FOR_THREAD: Cell<Option<bool>> = const { Cell::new(None) };
    /// Set by [`quietly`]: a report is listed but not printed.
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Starts listening to the lookups of the workspace folder (once per process).
pub fn install() {
    INSTALLED.call_once(|| aqua_mcp_adapters::browser_mcp::health::observe_workspace_root(heard));
}

fn heard(root: &Path, asked_at: &'static Location<'static>) {
    if ALLOWED.with(|a| a.get()) > 0 || !is_real_folder(root, &aqua_mcp_adapters::browser_mcp::health::natural_workspace_root()) {
        return;
    }
    let thread = std::thread::current();
    let strict = STRICT_FOR_THREAD.with(|s| s.get()).unwrap_or_else(strict_from_env);
    if let Err(sentence) = record(thread.name(), asked_at, strict) {
        panic!("{sentence}");
    }
}

fn strict_from_env() -> bool {
    std::env::var_os(STRICT_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Is `root` the real data folder (`real`), however the path is spelled (case, `..`, a trailing separator, a short name)?
pub fn is_real_folder(root: &Path, real: &Path) -> bool {
    fn plain(p: &Path) -> Vec<String> {
        let p = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let mut parts: Vec<String> = Vec::new();
        for part in p.components() {
            match part {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    parts.pop();
                }
                other => parts.push(other.as_os_str().to_string_lossy().trim_start_matches(r"\\?\").to_lowercase()),
            }
        }
        parts
    }
    plain(root) == plain(real)
}

/// Writes down that `thread` (a test, by its name) looked up the real folder at `asked_at`. `Err` carries the sentence that fails the test
/// when `strict` is on. Only the first report of each (test, place) pair is printed; the very first one also says what the line means.
pub fn record(thread: Option<&str>, asked_at: &Location<'_>, strict: bool) -> Result<(), String> {
    let who = thread.filter(|n| !n.is_empty() && *n != "main").map(str::to_string).unwrap_or_else(|| "an unnamed thread (spawned by a test)".to_string());
    let entry = format!("{who} at {}:{}", asked_at.file().replace('\\', "/"), asked_at.line());
    let (is_new, is_first) = {
        let mut seen = OFFENDERS.lock().unwrap_or_else(|e| e.into_inner());
        let is_new = !seen.contains(&entry);
        if is_new {
            seen.push(entry.clone());
        }
        (is_new, seen.len() == 1 && is_new)
    };
    if is_new && !QUIET.with(|q| q.get()) {
        use std::io::Write;
        let mut err = std::io::stderr();
        if is_first {
            let _ = writeln!(err, "[data tripwire] A test below used Aqua's real data folder. Give it one of its own: TempWorld::new(..).as_workspace_root(), or runtime::data_tripwire::with_data_dir. {STRICT_ENV}=1 makes it fail.");
        }
        let _ = writeln!(err, "REAL DATA FOLDER REACHED by {entry}");
    }
    if strict {
        Err(format!("the test `{who}` reached Aqua's real data folder at {}:{}; give it a folder of its own (TempWorld::new(..).as_workspace_root() or runtime::data_tripwire::with_data_dir) so that it cannot read or change the owner's data", asked_at.file().replace('\\', "/"), asked_at.line()))
    } else {
        Ok(())
    }
}

/// Runs `f` so that what it reports is listed by [`offenders`] but not printed. For the tests of the tripwire itself, whose pretend offenders must not look
/// like real ones in the output of a run.
pub fn quietly<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            QUIET.with(|q| q.set(self.0));
        }
    }
    let _restore = Restore(QUIET.with(|q| q.replace(true)));
    f()
}

/// Every (test, place) that has reached the real folder in this process so far.
pub fn offenders() -> Vec<String> {
    OFFENDERS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Runs `f` with the tripwire quiet on this thread: for a test that means to look at the real folder (it only resolves the path).
pub fn allow_real_data<R>(f: impl FnOnce() -> R) -> R {
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            ALLOWED.with(|a| a.set(a.get().saturating_sub(1)));
        }
    }
    ALLOWED.with(|a| a.set(a.get() + 1));
    let _leave = Leave;
    f()
}

/// Runs `f` so that, on this thread, `runtime::workspace_root()` is `root` (and `data_dir()` is `root/data`).
pub fn with_workspace_root<R>(root: &Path, f: impl FnOnce() -> R) -> R {
    overridden(|o| o.0 = Some(root.to_path_buf()), f)
}

/// Runs `f` so that, on this thread, `runtime::data_dir()` is `dir`.
pub fn with_data_dir<R>(dir: &Path, f: impl FnOnce() -> R) -> R {
    overridden(|o| o.1 = Some(dir.to_path_buf()), f)
}

fn overridden<R>(set: impl FnOnce(&mut (Option<PathBuf>, Option<PathBuf>)), f: impl FnOnce() -> R) -> R {
    struct Restore((Option<PathBuf>, Option<PathBuf>));
    impl Drop for Restore {
        fn drop(&mut self) {
            let before = std::mem::take(&mut self.0);
            OVERRIDE.with(|o| *o.borrow_mut() = before);
        }
    }
    let before = OVERRIDE.with(|o| o.borrow().clone());
    OVERRIDE.with(|o| set(&mut o.borrow_mut()));
    let _restore = Restore(before);
    f()
}

/// Decides, for this thread, whether a lookup of the real folder fails (`true`) or is only reported (`false`), whatever the environment says.
pub fn with_strictness<R>(strict: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<bool>);
    impl Drop for Restore {
        fn drop(&mut self) {
            STRICT_FOR_THREAD.with(|s| s.set(self.0));
        }
    }
    let _restore = Restore(STRICT_FOR_THREAD.with(|s| s.replace(Some(strict))));
    f()
}

pub(super) fn workspace_root_override() -> Option<PathBuf> {
    OVERRIDE.with(|o| o.borrow().0.clone())
}

pub(super) fn data_dir_override() -> Option<PathBuf> {
    OVERRIDE.with(|o| o.borrow().1.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempWorld;

    fn here() -> &'static Location<'static> {
        Location::caller()
    }

    #[test]
    fn the_real_folder_is_known_by_any_spelling_and_a_world_of_a_test_is_not() {
        let real = TempWorld::new("tripwire_real");
        let r = &real.root;
        assert!(is_real_folder(r, r));
        assert!(is_real_folder(&r.join("sub").join(".."), r), "a `..` does not hide it");
        assert!(is_real_folder(&PathBuf::from(r.to_string_lossy().to_uppercase()), r), "the case of the letters does not hide it");
        assert!(is_real_folder(&PathBuf::from(format!("{}{}", r.display(), std::path::MAIN_SEPARATOR)), r), "a trailing separator does not");
        let other = TempWorld::new("tripwire_other");
        assert!(!is_real_folder(&other.root, r));
        assert!(!is_real_folder(&r.join("data"), r), "the data folder below it is not the root: the root is what is compared");
    }

    #[test]
    fn a_report_names_the_test_and_the_place_and_in_strict_mode_fails_with_a_sentence() {
        let at = here();
        assert_eq!(quietly(|| record(Some("tripwire::report_probe"), at, false)), Ok(()));
        let listed = offenders();
        assert!(listed.iter().any(|o| o.starts_with("tripwire::report_probe at ") && o.contains("data_tripwire.rs:")), "{listed:?}");
        // The list is shared by every test of the process, and another test may reach the real folder and list itself at any moment: only this probe is counted,
        // and a second test is listed in between here, so that the race is not left to chance.
        let mine = || offenders().iter().filter(|o| o.starts_with("tripwire::report_probe at ")).count();
        let again = mine();
        quietly(|| record(Some("tripwire::another_test_meanwhile"), at, false)).unwrap();
        quietly(|| record(Some("tripwire::report_probe"), at, false)).unwrap();
        assert_eq!(mine(), again, "the same test at the same place is listed once, whatever other tests list meanwhile");
        let sentence = quietly(|| record(Some("tripwire::strict_probe"), at, true)).unwrap_err();
        assert!(sentence.starts_with("the test `tripwire::strict_probe` reached Aqua's real data folder at "), "{sentence}");
        assert!(sentence.contains("TempWorld"), "it says what to do: {sentence}");
    }

    #[test]
    fn a_thread_without_a_name_is_still_named_in_the_report() {
        quietly(|| record(None, here(), false)).unwrap();
        assert!(offenders().iter().any(|o| o.starts_with("an unnamed thread (spawned by a test) at ")));
    }

    #[test]
    fn a_lookup_of_the_real_folder_is_reported_and_one_of_a_world_is_not() {
        let natural = aqua_mcp_adapters::browser_mcp::health::natural_workspace_root();
        // The lookup is heard through the hook; here it is called as the hook calls it, on threads that carry the name of a test.
        let heard_on = |name: &str, root: PathBuf, strict: bool| {
            let probe = name.to_string();
            let outcome = std::thread::Builder::new()
                .name(probe.clone())
                .spawn(move || quietly(|| with_strictness(strict, || std::panic::catch_unwind(|| heard(&root, here())))))
                .unwrap()
                .join()
                .unwrap();
            (offenders().iter().any(|o| o.starts_with(&format!("{probe} at "))), outcome.err().map(|p| p.downcast_ref::<String>().cloned().unwrap_or_default()))
        };
        let (reported, failed) = heard_on("tripwire_probe_real", natural.clone(), false);
        assert!(reported && failed.is_none(), "reported without failing");
        let (reported, failed) = heard_on("tripwire_probe_strict", natural, true);
        assert!(reported, "reported");
        assert!(failed.is_some_and(|m| m.contains("`tripwire_probe_strict` reached Aqua's real data folder")), "and failed in strict mode, naming the test");
        let world = TempWorld::new("tripwire_world");
        let (reported, failed) = heard_on("tripwire_probe_world", world.root.clone(), true);
        assert!(!reported && failed.is_none(), "a world of the test's own is never reported, strict or not");
    }

    /// The wiring: the first `runtime::workspace_root()` of a test binary installs the hook. (The environment is left alone: a test that set
    /// `AQUA_WORKSPACE_ROOT` here would change what every test running beside it reads from `data/`.)
    #[test]
    fn the_first_lookup_installs_the_hook() {
        std::thread::Builder::new()
            .name("tripwire_probe_wiring".into())
            .spawn(|| {
                allow_real_data(|| {
                    crate::runtime::workspace_root();
                })
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(INSTALLED.is_completed(), "the lookup installed the hook");
    }

    #[test]
    fn a_test_that_means_to_look_at_the_real_folder_can_say_so() {
        let natural = aqua_mcp_adapters::browser_mcp::health::natural_workspace_root();
        let again = natural.clone();
        let result = std::thread::Builder::new()
            .name("tripwire_probe_allowed".into())
            .spawn(move || quietly(|| with_strictness(true, || allow_real_data(|| heard(&natural, here())))))
            .unwrap()
            .join();
        assert!(result.is_ok(), "allowed on this thread, even in strict mode");
        assert!(!offenders().iter().any(|o| o.starts_with("tripwire_probe_allowed at ")));
        // And it is only for the length of the call.
        let after = std::thread::Builder::new()
            .name("tripwire_probe_not_allowed_after".into())
            .spawn(move || {
                allow_real_data(|| ());
                quietly(|| with_strictness(true, || std::panic::catch_unwind(|| heard(&again, here()))))
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(after.is_err(), "the permission ended with the call");
    }

    #[test]
    fn a_thread_can_be_given_its_own_workspace_root_and_data_folder() {
        let base = TempWorld::new("tripwire_base");
        let world = TempWorld::new("tripwire_override");
        let data = world.path("elsewhere");
        with_workspace_root(&base.root, || {
            assert_eq!(crate::runtime::data_dir(), base.root.join("data"));
            with_workspace_root(&world.root, || {
                assert_eq!(crate::runtime::workspace_root(), world.root);
                assert_eq!(crate::runtime::data_dir(), world.root.join("data"));
                with_data_dir(&data, || {
                    assert_eq!(crate::runtime::data_dir(), data);
                    assert_eq!(crate::runtime::workspace_root(), world.root, "only the data folder moved");
                });
                assert_eq!(crate::runtime::data_dir(), world.root.join("data"), "and it came back");
            });
            assert_eq!(crate::runtime::data_dir(), base.root.join("data"), "the inner override ended with its call");
            let panicked = std::panic::catch_unwind(|| with_data_dir(&data, || panic!("a test failed inside")));
            assert!(panicked.is_err());
            assert_eq!(crate::runtime::data_dir(), base.root.join("data"), "also after a panic");
            // Another thread never sees it.
            let (root_there, data_there) = std::thread::spawn(|| (workspace_root_override(), data_dir_override())).join().unwrap();
            assert_eq!((root_there, data_there), (None, None));
        });
        assert_eq!((workspace_root_override(), data_dir_override()), (None, None), "nothing is left on this thread");
    }
}
